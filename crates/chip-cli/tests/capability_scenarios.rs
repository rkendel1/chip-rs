//! Capability composition: do the capabilities `chip work` really offers support bounded work?
//!
//! Each scenario runs the product entry point (`run_software_work_with_budget` with the product
//! policy and context policy) over the real `LocalEnvironment`: real files, real `git`, real PAX and
//! cargo. Only the model is scripted, and the script also records every request it was sent, so a
//! scenario can say what reality showed the model. Nothing is asserted about a model's skill.
//! If PAX is not installed a scenario says SKIPPED and returns.
//!
//! The outcomes asserted here are the audit's findings (see `docs/product/capabilities.md`),
//! including the ones that are limits rather than successes.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use chip_cli::local_environment::{LocalEnvironment, opaque_id};
use chip_cli::software_work::{
    CompleteWhenVerified, GoalKind, SoftwareWork, run_software_work_kind,
    run_software_work_with_budget,
};
use chip_core::{
    EnvironmentDescription, ExecutionEvent, LocalWorkPolicy, NoLocalPolicy, ObservationKind,
    WorkEvent, WorkId, WorkLimits, WorkOutcome,
};
use chip_pax::PaxExecutor;
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

const OLD_LIB: &str = "pub fn len(s: &str) -> usize {\n    s.len()\n}\n";
const RIGHT: &str = "pub fn len(s: &str) -> usize {\n    s.len()\n}\n\npub fn canonical(s: &str) -> String {\n    let mut p: Vec<&str> = s.split('&').collect();\n    p.sort();\n    p.join(\"&\")\n}\n";
const WRONG: &str = "pub fn len(s: &str) -> usize {\n    s.len()\n}\n\npub fn canonical(s: &str) -> String {\n    s.to_string()\n}\n";
const FAILING_TEST: &str = "use auditfx::canonical;\n\n#[test]\nfn pairs_are_sorted() {\n    assert_eq!(canonical(\"b=2&a=1\"), \"a=1&b=2\");\n}\n";
const PASSING_TEST: &str =
    "use auditfx::len;\n\n#[test]\nfn length() {\n    assert_eq!(len(\"abc\"), 3);\n}\n";
const GOAL: &str =
    "Add `canonical(s)` to src/lib.rs, which sorts the `&`-separated pairs, so the tests pass.";

struct Script {
    replies: Mutex<VecDeque<String>>,
    seen: Mutex<Vec<String>>,
}

impl Script {
    fn new(replies: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            seen: Mutex::new(Vec::new()),
        })
    }
    /// Everything the model was sent on call `n` (0-based).
    fn request(&self, n: usize) -> String {
        self.seen.lock().unwrap()[n].clone()
    }
    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl ModelProvider for Script {
    async fn complete(&self, r: ModelRequest) -> Result<ModelResponse, FxError> {
        self.seen.lock().unwrap().push(
            r.messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| FxError::Provider("the script ran out of replies".into()))?;
        Ok(ModelResponse::new("m", reply, Usage::new(1, 1)))
    }
}

fn q(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}
fn request(capability: &str, inputs: &str) -> String {
    if inputs.is_empty() {
        format!(r#"{{"decision":"request_capability","capability":"{capability}"}}"#)
    } else {
        format!(
            r#"{{"decision":"request_capability","capability":"{capability}","inputs":{inputs}}}"#
        )
    }
}
fn list(path: &str) -> String {
    request("project.list", &format!(r#"{{"path":{}}}"#, q(path)))
}
fn search(query: &str) -> String {
    request("project.search", &format!(r#"{{"query":{}}}"#, q(query)))
}
fn read(path: &str) -> String {
    request("project.read", &format!(r#"{{"path":{}}}"#, q(path)))
}
fn write(path: &str, content: &str) -> String {
    request(
        "project.write",
        &format!(r#"{{"path":{},"content":{}}}"#, q(path), q(content)),
    )
}
fn observe(scope: &str) -> String {
    request("project.observe", &format!(r#"{{"scope":{}}}"#, q(scope)))
}
fn pax_test() -> String {
    request("pax.test", "")
}
fn complete(summary: &str) -> String {
    format!(r#"{{"decision":"complete","summary":{}}}"#, q(summary))
}
fn block(reason: &str) -> String {
    format!(r#"{{"decision":"block","reason":{}}}"#, q(reason))
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

/// A real Rust project in a real Git repository with one commit.
fn project(tag: &str, test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chip-scenario-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"auditfx\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(dir.join(".gitignore"), "target\nCargo.lock\n").unwrap();
    std::fs::write(dir.join("src/lib.rs"), OLD_LIB).unwrap();
    std::fs::write(dir.join("tests/t.rs"), test).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "initial"]);
    dir
}

fn pax_available(dir: &Path) -> bool {
    let ok = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(PaxExecutor::new(dir).resolve())
    })
    .is_ok();
    if !ok {
        eprintln!("SKIPPED: PAX is not installed");
    }
    ok
}

/// `project.observe` needs the PAX release it was verified against; a test that needs it says
/// SKIPPED when the installed PAX is older, exactly as one that needs PAX does when it is absent.
fn observe_available(dir: &Path) -> bool {
    let ok = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(PaxExecutor::new(dir).resolve())
    })
    .is_ok_and(|pax| {
        chip_pax::MIN_OBSERVE_PAX_VERSION <= {
            let parts: Vec<u64> = pax
                .version
                .split(['.', '-', '+'])
                .take(3)
                .filter_map(|p| p.parse().ok())
                .collect();
            (parts[0], parts[1], parts[2])
        }
    });
    if !ok {
        eprintln!(
            "SKIPPED: PAX is older than {:?}",
            chip_pax::MIN_OBSERVE_PAX_VERSION
        );
    }
    ok
}

const LIMITS: WorkLimits = WorkLimits {
    max_turns: 14,
    max_executions: 10,
};

async fn run(dir: &Path, replies: Vec<String>) -> (SoftwareWork, Arc<Script>) {
    let env = LocalEnvironment::new(
        opaque_id(dir),
        dir,
        PaxExecutor::new(dir),
        EnvironmentDescription::default(),
    );
    let model = Script::new(replies);
    let work = run_software_work_with_budget(
        WorkId::new("scenario"),
        model.clone(),
        "scripted".into(),
        &env,
        GOAL,
        LIMITS,
        &CompleteWhenVerified,
        None,
    )
    .await;
    (work, model)
}

/// A run of the given kind. `Chip's own policy` for the kind decides locally what Chip may decide;
/// `policy` overrides it where a test needs the model, not the runtime, to be the one that finishes.
async fn run_kind(
    dir: &Path,
    kind: GoalKind,
    policy: Option<&dyn LocalWorkPolicy>,
    replies: Vec<String>,
) -> (SoftwareWork, Arc<Script>) {
    let env = LocalEnvironment::new(
        opaque_id(dir),
        dir,
        PaxExecutor::new(dir),
        EnvironmentDescription::default(),
    );
    let model = Script::new(replies);
    let work = run_software_work_kind(
        kind,
        WorkId::new("scenario"),
        model.clone(),
        "scripted".into(),
        &env,
        match kind {
            GoalKind::Change => GOAL,
            GoalKind::Verify => "Determine whether the project currently passes its tests.",
            GoalKind::Inspect => "Find where `len` is defined and report it.",
        },
        LIMITS,
        policy.unwrap_or_else(|| kind.policy()),
        None,
    )
    .await;
    (work, model)
}

fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            let rel = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if rel == "target" || rel == ".git" || rel == "Cargo.lock" {
                continue;
            }
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn started(w: &SoftwareWork) -> usize {
    w.report
        .events
        .iter()
        .filter(|e| {
            matches!(
                e,
                WorkEvent::Execution(ExecutionEvent::ExecutionStarted { .. })
            )
        })
        .count()
}

fn describe(w: &SoftwareWork) -> String {
    format!(
        "outcome={:?} executions={} goal_satisfied={:?} verified={}",
        w.report.outcome,
        started(w),
        w.goal_satisfied,
        w.verified
    )
}

/// The invariant for every goal kind: a completed run has met its kind's required condition at the
/// last evaluation (a level, `remaining == 0`), whichever turn produced the last evaluation event.
fn assert_completed_means_goal_met(w: &SoftwareWork) {
    if matches!(w.report.outcome, WorkOutcome::Completed { .. }) {
        assert_eq!(
            w.goal_satisfied,
            Some(true),
            "completed but the goal was not met: {}",
            describe(w)
        );
    }
}

/// Useful work is verified completion: one verified goal over the cost of the run, for a run that
/// verified; and for every other run, none, however much the goal's condition held.
fn assert_useful_work_is_one_verified_goal(w: &SoftwareWork) {
    assert!(w.verified, "{}", describe(w));
    assert_eq!(
        w.useful_work_per_model_call(),
        Some(1.0 / w.utility.model_calls as f64)
    );
    assert_eq!(
        w.useful_work_per_execution(),
        Some(1.0 / w.utility.executions as f64)
    );
}

fn assert_no_useful_work(w: &SoftwareWork) {
    assert!(!w.verified, "{}", describe(w));
    assert_eq!(w.useful_work_per_model_call(), Some(0.0), "{}", describe(w));
    assert_eq!(w.useful_work_per_execution(), Some(0.0), "{}", describe(w));
    let json: serde_json::Value = serde_json::from_str(&chip_cli::software_work::render_json(
        w,
        &EnvironmentDescription::default(),
    ))
    .unwrap();
    assert_eq!(json["useful_work_per_model_call"], 0.0);
    assert_eq!(json["useful_work_per_execution"], 0.0);
}

/// `GoalEvaluated::satisfied` of the last evaluation: an edge ("this observation produced it"),
/// which is not the level `goal_satisfied` reports.
fn last_goal_edge(w: &SoftwareWork) -> Option<bool> {
    w.report.events.iter().rev().find_map(|e| match e {
        WorkEvent::GoalEvaluated { satisfied, .. } => Some(*satisfied),
        _ => None,
    })
}

fn blocked_with(w: &SoftwareWork, text: &str) -> bool {
    matches!(&w.report.outcome, WorkOutcome::Blocked { reason } if reason.contains(text))
}

fn last_output(w: &SoftwareWork) -> &str {
    w.report
        .observations
        .last()
        .and_then(|o| o.output.as_deref())
        .expect("an observation")
}

// ---- the five scenarios -----------------------------------------------------------------------------

/// Scenario 1, as change work (the default kind). Inspecting works, but a change goal is only
/// satisfied by "a file changed and PAX passed after it", so a claim of completion that changed
/// nothing is refused. Read-only goals have their own kind: see
/// `inspect_completes_on_a_grounded_answer_without_a_change_or_a_test`.
#[tokio::test(flavor = "multi_thread")]
async fn scenario_1_inspect_can_observe_but_not_complete() {
    let dir = project("s1", FAILING_TEST);
    if !pax_available(&dir) {
        return;
    }
    let before = snapshot(&dir);

    let (w, m) = run(
        &dir,
        vec![
            list("."),
            search("len"),
            read("src/lib.rs"),
            complete("len is defined in src/lib.rs"),
        ],
    )
    .await;
    assert_eq!(
        started(&w),
        3,
        "list, search and read really ran: {}",
        describe(&w)
    );
    assert!(
        m.request(3).contains("pub fn len"),
        "the model was shown the file"
    );
    assert!(
        blocked_with(&w, "completion refused"),
        "a completion with no change and no passing test is refused: {}",
        describe(&w)
    );
    assert_ne!(w.goal_satisfied, Some(true));
    assert_eq!(before, snapshot(&dir), "inspection changed nothing");

    // The only way to hand a finding back is the reason of a block (or an escalation).
    let (w, _) = run(
        &dir,
        vec![
            list("."),
            read("src/lib.rs"),
            block("finding: len lives in src/lib.rs"),
        ],
    )
    .await;
    assert!(
        blocked_with(&w, "finding: len lives in src/lib.rs"),
        "{}",
        describe(&w)
    );
    assert_eq!(before, snapshot(&dir));
}

/// Scenario 2. Search, read, write, look at the diff, verify, and the runtime completes the work.
#[tokio::test(flavor = "multi_thread")]
async fn scenario_2_modify_diff_verify_complete() {
    let dir = project("s2", FAILING_TEST);
    if !pax_available(&dir) {
        return;
    }
    let (w, m) = run(
        &dir,
        vec![
            search("len"),
            read("src/lib.rs"),
            write("src/lib.rs", RIGHT),
            request("project.git.diff", ""),
            pax_test(),
        ],
    )
    .await;
    assert!(
        matches!(w.report.outcome, WorkOutcome::Completed { .. }),
        "{}",
        describe(&w)
    );
    assert!(w.verified && w.goal_satisfied == Some(true));
    assert_completed_means_goal_met(&w);
    assert_useful_work_is_one_verified_goal(&w);
    assert_eq!(
        m.calls(),
        5,
        "the runtime completed the work itself; the model made no completion call"
    );
    // The diff the model saw is the real diff of the real change.
    let diff = m.request(4);
    assert!(
        diff.contains("+pub fn canonical") && diff.contains("unstaged diff"),
        "{diff}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
        RIGHT
    );
    w.audit.assert_clean();
}

/// Scenario 3. A wrong attempt fails for real; PAX's diagnostics reach the model; a second attempt
/// passes. Recovery needed no capability beyond the existing ones.
#[tokio::test(flavor = "multi_thread")]
async fn scenario_3_repair_after_a_failed_test() {
    let dir = project("s3", FAILING_TEST);
    if !pax_available(&dir) {
        return;
    }
    let (w, m) = run(
        &dir,
        vec![
            read("src/lib.rs"),
            write("src/lib.rs", WRONG),
            pax_test(),
            write("src/lib.rs", RIGHT),
            pax_test(),
        ],
    )
    .await;
    assert!(
        matches!(w.report.outcome, WorkOutcome::Completed { .. }),
        "{}",
        describe(&w)
    );
    assert!(w.verified);
    // What the model was shown after the failure: PAX's verdict and the native diagnostics.
    let evidence = m.request(3);
    assert!(
        evidence.contains("\\\"status\\\":\\\"failed\\\""),
        "PAX's status: {evidence}"
    );
    assert!(
        evidence.contains("assertion `left == right` failed"),
        "the failing assertion: {evidence}"
    );
    assert!(
        evidence.contains("pairs_are_sorted"),
        "the failing test's name: {evidence}"
    );
    assert!(
        evidence.contains("pax.test: its execution failed"),
        "ruled out, by Chip: {evidence}"
    );
    assert_eq!(
        w.utility.recoveries, 1,
        "one failed observation was followed by further work"
    );
    let failed = w
        .report
        .observations
        .iter()
        .filter(|o| o.kind == ObservationKind::ExecutionFailed)
        .count();
    assert_eq!(failed, 1);
}

/// Scenario 4. A capability that does not exist cannot be requested, and nothing is substituted.
#[tokio::test(flavor = "multi_thread")]
async fn scenario_4_an_undeclared_capability_executes_nothing_and_is_not_substituted() {
    let dir = project("s4", FAILING_TEST);
    if !pax_available(&dir) {
        return;
    }
    let before = snapshot(&dir);
    for (name, wanted) in [
        ("shell.exec", r#"{"command":"ls"}"#),
        ("project.delete", r#"{"path":"src/lib.rs"}"#),
        ("git.commit", r#"{"message":"x"}"#),
        ("http.get", r#"{"url":"https://example.com"}"#),
    ] {
        let (w, m) = run(
            &dir,
            vec![list("."), request(name, wanted), write("src/lib.rs", RIGHT)],
        )
        .await;
        assert!(
            matches!(&w.report.outcome, WorkOutcome::Failed { reason } if reason.contains("unknown capability")),
            "{name}: {}",
            describe(&w)
        );
        assert_eq!(
            started(&w),
            1,
            "{name}: only the earlier list ran; nothing was substituted"
        );
        // A model asking for something that does not exist is a failed decision, ended closed:
        // it exits as a runtime failure (4), not as "not verified" (1). Recorded as a finding.
        assert_eq!(
            w.exit_status(),
            chip_cli::verify::EXIT_RUNTIME_FAILURE,
            "{name}"
        );
        assert_eq!(m.calls(), 2, "{name}: no further model call, no retry");
        assert_eq!(before, snapshot(&dir), "{name}: nothing changed");
    }
}

/// Scenario 4, continued: a capability that exists but is asked for outside its authority is
/// refused before anything runs, and a model that sees the refusal can block.
#[tokio::test(flavor = "multi_thread")]
async fn scenario_4_a_declared_capability_outside_its_authority_is_refused_before_it_runs() {
    let dir = project("s4b", FAILING_TEST);
    if !pax_available(&dir) {
        return;
    }
    let before = snapshot(&dir);
    for (what, bad) in [
        (
            "write into .git",
            write(".git/hooks/pre-commit", "#!/bin/sh\n"),
        ),
        ("write outside the root", write("../escape.txt", "x")),
        ("write an absolute path", write("/tmp/escape.txt", "x")),
        ("write a secret file", write(".env", "K=V")),
        ("read outside the root", read("../Cargo.toml")),
    ] {
        let (w, _) = run(&dir, vec![bad]).await;
        assert!(
            blocked_with(&w, "invalid capability input"),
            "{what}: {}",
            describe(&w)
        );
        assert_eq!(started(&w), 0, "{what}: nothing executed");
        assert_eq!(
            w.exit_status(),
            chip_cli::verify::EXIT_NOT_VERIFIED,
            "{what}"
        );
        assert_eq!(before, snapshot(&dir), "{what}");
    }
    assert!(!dir.parent().unwrap().join("escape.txt").exists());
}

/// Scenario 5, as change work (the default kind). A change goal needs a content-changing write
/// followed by a passing test, so a project that already passes is not "completed" by claiming it.
/// The honest no-op is a verify goal: see
/// `an_already_satisfied_project_completes_as_a_verify_goal_not_through_a_fake_write`.
#[tokio::test(flavor = "multi_thread")]
async fn scenario_5_a_noop_is_verified_but_cannot_complete() {
    let dir = project("s5", PASSING_TEST);
    if !pax_available(&dir) {
        return;
    }
    let before = snapshot(&dir);
    let (w, m) = run(
        &dir,
        vec![list("."), pax_test(), complete("the tests already pass")],
    )
    .await;
    assert!(
        m.request(2).contains("\\\"status\\\":\\\"passed\\\""),
        "reality shows the tests pass"
    );
    assert!(blocked_with(&w, "completion refused"), "{}", describe(&w));
    assert_eq!(
        w.goal_satisfied,
        Some(false),
        "no change was made, so the goal predicate is unmet"
    );
    assert_eq!(before, snapshot(&dir), "no unnecessary mutation");
    // Rewriting the same bytes is not a change either.
    let (w, _) = run(
        &dir,
        vec![write("src/lib.rs", OLD_LIB), pax_test(), complete("done")],
    )
    .await;
    assert!(blocked_with(&w, "completion refused"), "{}", describe(&w));
    assert_eq!(before, snapshot(&dir));
}

// ---- limits the scenarios exposed ---------------------------------------------------------------------

/// Observations carry structured, recoverable failures, and the limits are real.
#[tokio::test(flavor = "multi_thread")]
async fn capability_limits_are_observed_as_structured_failures_not_hidden() {
    let dir = project("limits", FAILING_TEST);
    if !pax_available(&dir) {
        return;
    }
    std::fs::write(dir.join("big.txt"), "x".repeat(40 * 1024)).unwrap();
    std::fs::write(dir.join("bin.dat"), [0xffu8, 0xfe, 0x00, 0x80]).unwrap();

    let observed = |replies: Vec<String>| {
        let dir = dir.clone();
        async move {
            let (w, _) = run(&dir, replies).await;
            let out = last_output(&w).to_string();
            (w, out)
        }
    };
    // A file over 32 KiB cannot be read at all (no ranged read): the failure says so.
    let (w, out) = observed(vec![read("big.txt"), block("stop")]).await;
    assert!(out.contains("\"error\":\"too_large\""), "{out}");
    assert!(w.report.observations[0].kind == ObservationKind::ExecutionFailed);
    // Non-UTF-8 content is refused as such.
    let (_, out) = observed(vec![read("bin.dat"), block("stop")]).await;
    assert!(out.contains("\"error\":\"not_utf8\""), "{out}");
    // A file cannot be created in a directory that does not exist, and no capability creates one.
    let (_, out) = observed(vec![write("newdir/mod.rs", "// x"), block("stop")]).await;
    assert!(out.contains("\"error\":\"parent_missing\""), "{out}");
    assert!(!dir.join("newdir").exists());
    // Content over 32 KiB is rejected before anything is written.
    let (w, _) = run(&dir, vec![write("src/huge.rs", &"y".repeat(40 * 1024))]).await;
    assert_eq!(started(&w), 0);
    assert!(!dir.join("src/huge.rs").exists());
    // A path with a character outside the allowed set is refused, whatever the file system allows.
    let (w, _) = run(&dir, vec![write("src/my file.rs", "x")]).await;
    assert_eq!(started(&w), 0, "{}", describe(&w));
    // Search is a literal substring, not a pattern.
    let (_, out) = observed(vec![search("l.n"), block("stop")]).await;
    assert!(out.contains("\"matches\":0"), "{out}");
}

/// Git observation: a new file is visible to `status` and invisible to `diff`, which says so.
#[tokio::test(flavor = "multi_thread")]
async fn git_status_shows_a_new_file_but_the_diff_does_not() {
    let dir = project("git", FAILING_TEST);
    if !pax_available(&dir) {
        return;
    }
    let (w, m) = run(
        &dir,
        vec![
            write("src/extra.rs", "pub fn extra() {}\n"),
            request("project.git.status", ""),
            request("project.git.diff", ""),
            request("project.git.diff_stat", ""),
            request("project.git.log", r#"{"count":5}"#),
            block("stop"),
        ],
    )
    .await;
    let obs: Vec<&str> = w
        .report
        .observations
        .iter()
        .filter_map(|o| o.output.as_deref())
        .collect();
    assert!(
        obs[1].contains("src/extra.rs") && obs[1].contains("untracked"),
        "{}",
        obs[1]
    );
    assert!(
        obs[2].contains("\"includes_untracked\":false") && !obs[2].contains("pub fn extra"),
        "{}",
        obs[2]
    );
    assert!(
        obs[4].contains("initial"),
        "the log shows the real commit: {}",
        obs[4]
    );
    assert_eq!(m.calls(), 6);
}

/// The authority a model has through `project.write` + `pax.test` is more than file access: the
/// project's own test tooling runs what the model wrote, with the tools' permissions. Chip bounds
/// the files the model may touch, not what the project's tests do (stated in the README). This test
/// records that fact; if a sandbox is ever added, this is the test that should change.
#[tokio::test(flavor = "multi_thread")]
async fn write_plus_test_is_code_execution_through_the_projects_own_tooling() {
    let dir = project("exec", FAILING_TEST);
    if !pax_available(&dir) {
        return;
    }
    let marker = dir
        .parent()
        .unwrap()
        .join(format!("chip-scenario-marker-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let test = format!(
        "#[test]\nfn leaves_a_marker() {{\n    std::fs::write({:?}, \"ran\").unwrap();\n}}\n",
        marker.display().to_string()
    );
    let (w, _) = run(
        &dir,
        vec![
            write("tests/t.rs", &test),
            write("src/lib.rs", RIGHT),
            pax_test(),
        ],
    )
    .await;
    assert!(w.verified, "{}", describe(&w));
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "ran",
        "the model's code ran, outside the project root"
    );
    let _ = std::fs::remove_file(&marker);
}

// ---- goal-aware completion ----------------------------------------------------------------------------
//
// The model proposes; Chip evaluates; reality provides the evidence; only Chip establishes
// completion. Each kind below is judged from observations, and a model's claim never completes
// anything by itself.

fn pax_passed(w: &SoftwareWork) -> usize {
    w.report
        .observations
        .iter()
        .filter(|o| {
            o.output
                .as_deref()
                .is_some_and(|t| t.contains("\"status\":\"passed\""))
        })
        .count()
}

/// Inspect: observations, then an answer that cites what was observed. Chip accepts it; the
/// model's claim is checked against the observations and was not what completed the work.
#[tokio::test(flavor = "multi_thread")]
async fn inspect_completes_on_a_grounded_answer_without_a_change_or_a_test() {
    let dir = project("inspect-ok", FAILING_TEST);
    if !pax_available(&dir) {
        return;
    }
    let before = snapshot(&dir);
    let (w, m) = run_kind(
        &dir,
        GoalKind::Inspect,
        None,
        vec![
            list("."),
            search("len"),
            read("src/lib.rs"),
            complete("`len` is defined in src/lib.rs (line 1) and returns the byte length."),
        ],
    )
    .await;
    assert!(
        matches!(&w.report.outcome, WorkOutcome::Completed { summary } if summary.contains("src/lib.rs")),
        "{}",
        describe(&w)
    );
    assert_eq!(
        started(&w),
        3,
        "three real observations, nothing else: {}",
        describe(&w)
    );
    assert_eq!(w.pax_executions(), 0, "no test was needed to answer");
    assert!(w.grounded, "grounded in what was observed");
    assert!(
        !w.verified,
        "grounded is not verified: nothing independent establishes the answer"
    );
    assert_eq!(w.goal_satisfied, Some(true), "the kind's condition was met");
    assert_completed_means_goal_met(&w);
    assert_eq!(
        w.exit_status(),
        chip_cli::verify::EXIT_NOT_VERIFIED,
        "only `verified` authorizes exit 0"
    );
    assert_eq!(before, snapshot(&dir), "nothing changed");
    assert!(
        m.request(3).contains("pub fn len"),
        "the model was shown the file it cites"
    );
    w.audit.assert_clean();
}

/// E1 from the real-model run: a negative answer that cites files Chip really read. Chip does not
/// interpret the answer, so it is grounded and never verified. Many observations follow the one that
/// first met the condition, so the last `GoalEvaluated` edge is false while the level is true.
#[tokio::test(flavor = "multi_thread")]
async fn a_negative_inspect_answer_citing_read_files_is_grounded_and_never_verified() {
    let dir = project("inspect-negative", FAILING_TEST);
    if !pax_available(&dir) {
        return;
    }
    let before = snapshot(&dir);
    let (mut w, _) = run_kind(
        &dir,
        GoalKind::Inspect,
        None,
        vec![
            list("."),
            read("Cargo.toml"),
            search("production"),
            read("src/lib.rs"),
            complete(
                "There is no information about which version is deployed to production; \
                 Cargo.toml and src/lib.rs contain nothing about it.",
            ),
        ],
    )
    .await;
    assert!(
        matches!(w.report.outcome, WorkOutcome::Completed { .. }),
        "{}",
        describe(&w)
    );
    assert_eq!(
        last_goal_edge(&w),
        Some(false),
        "the later observations did not newly produce the condition"
    );
    assert_eq!(
        w.goal_satisfied,
        Some(true),
        "but the condition held: goal_satisfied is a level"
    );
    assert_completed_means_goal_met(&w);
    assert!(w.grounded, "it cites files Chip observed");
    assert!(!w.verified, "no independent predicate established it");
    assert_eq!(w.exit_status(), chip_cli::verify::EXIT_NOT_VERIFIED);
    assert_no_useful_work(&w);
    assert_eq!(before, snapshot(&dir));
    w.audit.assert_clean();

    // Grounded is not another way to succeed: a completed inspection that is not grounded is the
    // runtime's own contradiction, not a not-verified answer.
    w.grounded = false;
    assert_eq!(w.exit_status(), chip_cli::verify::EXIT_RUNTIME_FAILURE);
}

/// An inspection that observes plenty and never answers: the goal's condition (something was
/// observed) holds, but nothing was verified, so no useful work was done.
#[tokio::test(flavor = "multi_thread")]
async fn an_inspection_that_hits_a_limit_did_no_useful_work() {
    let dir = project("inspect-limit", FAILING_TEST);
    if !pax_available(&dir) {
        return;
    }
    let (w, _) = run_kind(
        &dir,
        GoalKind::Inspect,
        None,
        (0..LIMITS.max_executions + 1).map(|_| list(".")).collect(),
    )
    .await;
    assert!(
        matches!(w.report.outcome, WorkOutcome::LimitReached { .. }),
        "{}",
        describe(&w)
    );
    assert_eq!(
        w.goal_satisfied,
        Some(true),
        "the observation condition held"
    );
    assert!(!w.grounded && !w.verified);
    assert_no_useful_work(&w);
    w.audit.assert_clean();
}

/// A' from the real-model run: a search row shows the signature, never the body that holds the
/// fact. Citing the file is grounded; it does not make the answer verified.
#[tokio::test(flavor = "multi_thread")]
async fn citing_a_search_row_without_observing_the_supporting_bytes_is_not_verified() {
    let dir = project("inspect-row", FAILING_TEST);
    if !pax_available(&dir) {
        return;
    }
    std::fs::write(
        dir.join("src/facts.rs"),
        "/// Expired sessions are purged after this many days.\npub fn purge_window() -> u32 {\n    let base = 30;\n    let grace = 5;\n    base + grace\n}\n",
    )
    .unwrap();
    let (w, _) = run_kind(
        &dir,
        GoalKind::Inspect,
        None,
        vec![
            search("purge_window"),
            complete("`purge_window` returns a number of days; it is defined in src/facts.rs."),
        ],
    )
    .await;
    assert!(
        w.report.observations.iter().all(|o| !o
            .output
            .as_deref()
            .unwrap_or_default()
            .contains("grace")),
        "the body that holds the fact was never observed"
    );
    assert!(matches!(w.report.outcome, WorkOutcome::Completed { .. }));
    assert!(w.grounded, "the answer cites a file the search matched");
    assert!(!w.verified);
    assert_completed_means_goal_met(&w);
    assert_eq!(w.exit_status(), chip_cli::verify::EXIT_NOT_VERIFIED);
    assert_no_useful_work(&w);
    // The seam: for an inspection `verified` is false whatever was observed, until an
    // independently owned predicate exists.
    assert!(!GoalKind::Inspect.verified(&w.report.observations, &w.report.outcome));
    w.audit.assert_clean();
}

/// Inspect: a claim reality does not support is refused, and the refusal is the runtime's.
#[tokio::test(flavor = "multi_thread")]
async fn inspect_refuses_an_answer_that_is_not_grounded_in_the_observations() {
    let dir = project("inspect-bad", FAILING_TEST);
    if !pax_available(&dir) {
        return;
    }
    let before = snapshot(&dir);
    let observe = || vec![list("."), read("src/lib.rs")];
    let with = |answer: &str| {
        let mut replies = observe();
        replies.push(complete(answer));
        replies
    };
    for (what, answer) in [
        ("cites nothing", "It is defined somewhere in the library."),
        (
            "cites a file that was not observed",
            "It is in src/other.rs.",
        ),
        ("cites only part of a longer path", "It is in mysrc/lib.rs."),
        ("cites a longer extension", "It is in src/lib.rs2."),
        ("is empty", "   "),
    ] {
        let (w, _) = run_kind(&dir, GoalKind::Inspect, None, with(answer)).await;
        assert!(
            blocked_with(&w, "not supported by the observations"),
            "{what}: {}",
            describe(&w)
        );
        assert!(!w.verified, "{what}");
        assert_eq!(
            w.exit_status(),
            chip_cli::verify::EXIT_NOT_VERIFIED,
            "{what}"
        );
        assert_eq!(before, snapshot(&dir), "{what}");
    }
    // A listing alone does not ground a claim about a file that was never listed or read either.
    let (w, _) = run_kind(
        &dir,
        GoalKind::Inspect,
        None,
        vec![list("tests"), complete("It is in src/lib.rs.")],
    )
    .await;
    assert!(
        blocked_with(&w, "not supported by the observations"),
        "{}",
        describe(&w)
    );
}

/// Inspect: no observation, or a change, and the claim is refused however well it is worded.
#[tokio::test(flavor = "multi_thread")]
async fn inspect_requires_observation_and_forbids_change() {
    let dir = project("inspect-guard", FAILING_TEST);
    if !pax_available(&dir) {
        return;
    }
    let before = snapshot(&dir);
    // Nothing observed: there is nothing to ground an answer in.
    let (w, _) = run_kind(
        &dir,
        GoalKind::Inspect,
        None,
        vec![complete("src/lib.rs defines len.")],
    )
    .await;
    assert!(blocked_with(&w, "completion refused"), "{}", describe(&w));
    assert_eq!(started(&w), 0);
    assert_eq!(before, snapshot(&dir));
    // A change was made: this was not a read-only inspection, so it cannot complete as one.
    let (w, _) = run_kind(
        &dir,
        GoalKind::Inspect,
        None,
        vec![
            read("src/lib.rs"),
            write("src/lib.rs", RIGHT),
            complete("src/lib.rs defines len."),
        ],
    )
    .await;
    assert!(blocked_with(&w, "completion refused"), "{}", describe(&w));
    assert!(!w.verified);
}

/// `project.observe` is a fact about structure. The completion contract is unchanged by it: an
/// inspection that observed structure and nothing else is not grounded, because grounding is what
/// `list`, `search` and `read` observed. With a read of the file it cites, it is grounded as before.
#[tokio::test(flavor = "multi_thread")]
async fn project_observe_does_not_ground_an_answer_and_does_not_change_the_contract() {
    let dir = project("observe-inspect", FAILING_TEST);
    if !pax_available(&dir) || !observe_available(&dir) {
        return;
    }
    let before = snapshot(&dir);
    // Observation alone: it names the file and the declaration, and the answer cites them.
    let (w, m) = run_kind(
        &dir,
        GoalKind::Inspect,
        None,
        vec![
            observe("crate:auditfx"),
            complete("`len` is declared in src/lib.rs."),
        ],
    )
    .await;
    assert!(
        m.request(1).contains("pub fn len"),
        "the model was shown the observed declaration: {}",
        m.request(1)
    );
    assert!(
        blocked_with(&w, "completion refused"),
        "an observation alone does not complete an inspection: {}",
        describe(&w)
    );
    assert!(!w.verified && !w.grounded);
    assert_eq!(w.exit_status(), chip_cli::verify::EXIT_NOT_VERIFIED);
    assert_eq!(w.useful_work_per_model_call(), Some(0.0));
    assert_eq!(before, snapshot(&dir), "observing changed nothing");
    w.audit.assert_clean();

    // With a read of the file it cites: grounded, never verified, exit 1: exactly as without observe.
    let (w, _) = run_kind(
        &dir,
        GoalKind::Inspect,
        None,
        vec![
            observe("crate:auditfx"),
            read("src/lib.rs"),
            complete("`len` is declared in src/lib.rs."),
        ],
    )
    .await;
    assert!(
        matches!(w.report.outcome, WorkOutcome::Completed { .. }),
        "{}",
        describe(&w)
    );
    assert!(w.grounded && !w.verified, "grounded is not verified");
    assert_eq!(w.goal_satisfied, Some(true));
    assert_eq!(w.exit_status(), chip_cli::verify::EXIT_NOT_VERIFIED);
    assert_eq!(w.useful_work_per_model_call(), Some(0.0));
    w.audit.assert_clean();
}

/// An observation is not a model's claim and a model's claim is not an observation.
#[tokio::test(flavor = "multi_thread")]
async fn a_model_cannot_supply_an_observation_or_a_fact() {
    let dir = project("observe-forge", FAILING_TEST);
    if !pax_available(&dir) || !observe_available(&dir) {
        return;
    }
    // The model asks to observe and tries to bring its own facts, limits and state: refused before
    // PAX is started, and nothing becomes an observation.
    for inputs in [
        r#"{"scope":"crate:auditfx","facts":"src/lib.rs declares len"}"#,
        r#"{"scope":"crate:auditfx","max_files":100000}"#,
        r#"{"scope":"crate:auditfx","state":"complete"}"#,
        r#"{"scope":"crate:auditfx","observation":"complete"}"#,
        r#"{"facts":"src/lib.rs declares len"}"#,
    ] {
        let (w, _) = run_kind(
            &dir,
            GoalKind::Inspect,
            None,
            vec![
                request("project.observe", inputs),
                complete("`len` is declared in src/lib.rs."),
            ],
        )
        .await;
        assert!(
            blocked_with(&w, "invalid capability input"),
            "{inputs}: {}",
            describe(&w)
        );
        assert_eq!(started(&w), 0, "{inputs}: nothing ran");
        assert!(
            w.report.observations.is_empty(),
            "{inputs}: nothing was observed"
        );
    }
    // Claiming a structure that was never observed does not complete anything.
    let (w, _) = run_kind(
        &dir,
        GoalKind::Inspect,
        None,
        vec![complete(
            "src/lib.rs declares `len`, and src/other.rs declares `helper`.",
        )],
    )
    .await;
    assert!(blocked_with(&w, "completion refused"), "{}", describe(&w));
    assert!(w.report.observations.is_empty());
}

/// Observing structure is work: it costs model calls and executions, and it is not useful work. A
/// change that is verified is still the only thing that counts, and observation does not alter it.
#[tokio::test(flavor = "multi_thread")]
async fn observing_structure_is_cost_and_never_useful_work_by_itself() {
    let dir = project("observe-change", FAILING_TEST);
    if !pax_available(&dir) || !observe_available(&dir) {
        return;
    }
    let (w, _) = run(
        &dir,
        vec![
            observe("crate:auditfx"),
            read("src/lib.rs"),
            write("src/lib.rs", RIGHT),
            pax_test(),
        ],
    )
    .await;
    assert!(
        w.verified && w.goal_satisfied == Some(true),
        "{}",
        describe(&w)
    );
    assert_eq!(w.exit_status(), chip_cli::verify::EXIT_VERIFIED);
    assert_eq!(
        w.utility.executions_by_capability.get("project.observe"),
        Some(&1)
    );
    // The observation is in the cost: one verified goal over four executions, not three.
    assert_eq!(w.utility.executions, 4);
    assert_eq!(w.useful_work_per_execution(), Some(0.25));
    assert_completed_means_goal_met(&w);
    w.audit.assert_clean();

    // And an unverified run that observed plenty did no useful work.
    let (w, _) = run(
        &dir,
        vec![
            observe("crate:auditfx"),
            observe("file:src/lib.rs"),
            complete("done"),
        ],
    )
    .await;
    assert!(!w.verified);
    assert_eq!(w.useful_work_per_model_call(), Some(0.0));
}

/// Verify: the runtime completes the work itself when PAX passes an unchanged project. The model
/// makes one call and never claims completion.
#[tokio::test(flavor = "multi_thread")]
async fn verify_completes_when_the_unchanged_project_passes() {
    let dir = project("verify-ok", PASSING_TEST);
    if !pax_available(&dir) {
        return;
    }
    let before = snapshot(&dir);
    let (w, m) = run_kind(&dir, GoalKind::Verify, None, vec![pax_test()]).await;
    assert!(
        matches!(w.report.outcome, WorkOutcome::Completed { .. }),
        "{}",
        describe(&w)
    );
    assert!(w.verified && w.goal_satisfied == Some(true));
    assert_completed_means_goal_met(&w);
    assert_useful_work_is_one_verified_goal(&w);
    assert_eq!(pax_passed(&w), 1, "PAX really passed");
    assert_eq!((w.writes, w.changed_writes), (0, 0));
    assert_eq!(
        m.calls(),
        1,
        "the runtime completed it; the model claimed nothing"
    );
    assert_eq!(w.exit_status(), chip_cli::verify::EXIT_VERIFIED);
    assert_eq!(before, snapshot(&dir));
    w.audit.assert_clean();

    // The same with some inspection first; and an identical-bytes write changes nothing, so it
    // neither counts as a change nor as evidence.
    let (w, _) = run_kind(
        &dir,
        GoalKind::Verify,
        None,
        vec![
            list("."),
            read("src/lib.rs"),
            write("src/lib.rs", OLD_LIB),
            pax_test(),
        ],
    )
    .await;
    assert!(
        matches!(w.report.outcome, WorkOutcome::Completed { .. }),
        "{}",
        describe(&w)
    );
    assert_eq!(
        (w.writes, w.changed_writes),
        (1, 0),
        "the write changed no bytes"
    );
    assert_eq!(before, snapshot(&dir));
}

/// Verify: a failing project does not complete, whatever the model says afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn verify_does_not_complete_when_the_tests_fail() {
    let dir = project("verify-fail", FAILING_TEST);
    if !pax_available(&dir) {
        return;
    }
    let before = snapshot(&dir);
    let (w, m) = run_kind(
        &dir,
        GoalKind::Verify,
        None,
        vec![pax_test(), complete("Looks fine to me.")],
    )
    .await;
    assert!(
        !matches!(w.report.outcome, WorkOutcome::Completed { .. }),
        "{}",
        describe(&w)
    );
    assert!(blocked_with(&w, "completion refused"), "{}", describe(&w));
    assert_eq!(pax_passed(&w), 0);
    assert!(!w.verified && w.goal_satisfied == Some(false));
    // The failure reached the decision that followed it.
    assert!(
        m.request(1).contains("\\\"status\\\":\\\"failed\\\""),
        "{}",
        m.request(1)
    );
    assert!(
        m.request(1).contains("canonical"),
        "the compiler's diagnostics, naming what is missing, reached the model"
    );
    assert_eq!(w.exit_status(), chip_cli::verify::EXIT_NOT_VERIFIED);
    assert_eq!(before, snapshot(&dir));
}

/// A project that already satisfies a change goal needs no manufactured change: it is a verify
/// goal. As a change goal it still cannot complete, and an identical write is no shortcut.
#[tokio::test(flavor = "multi_thread")]
async fn an_already_satisfied_project_completes_as_a_verify_goal_not_through_a_fake_write() {
    let dir = project("noop", PASSING_TEST);
    if !pax_available(&dir) {
        return;
    }
    let before = snapshot(&dir);
    let (w, _) = run_kind(&dir, GoalKind::Verify, None, vec![list("."), pax_test()]).await;
    assert!(
        matches!(w.report.outcome, WorkOutcome::Completed { .. }),
        "{}",
        describe(&w)
    );
    assert_eq!((w.writes, w.changed_writes), (0, 0), "no write was needed");
    assert_eq!(before, snapshot(&dir));
    // As change work, passing without a change is not completion, and writing the same bytes is not
    // a change.
    let (w, _) = run_kind(
        &dir,
        GoalKind::Change,
        None,
        vec![write("src/lib.rs", OLD_LIB), pax_test()],
    )
    .await;
    assert!(
        !matches!(w.report.outcome, WorkOutcome::Completed { .. }),
        "{}",
        describe(&w)
    );
    assert!(!w.verified && w.changed_writes == 0);
    assert_eq!(before, snapshot(&dir));
}

/// Verification goes stale when the project changes after it. The model is made the one that asks
/// to finish (Chip's own completion is switched off), so only the evaluation can refuse it.
#[tokio::test(flavor = "multi_thread")]
async fn a_verification_cannot_complete_a_project_that_changed_after_it() {
    let dir = project("stale", FAILING_TEST);
    if !pax_available(&dir) {
        return;
    }
    // Change work: write, test (passes), write again, claim. The pass no longer covers the files.
    let (w, _) = run_kind(
        &dir,
        GoalKind::Change,
        Some(&NoLocalPolicy),
        vec![
            write("src/lib.rs", RIGHT),
            pax_test(),
            write(
                "src/lib.rs",
                &format!("{RIGHT}\n// edited after the test\n"),
            ),
            complete("the tests pass"),
        ],
    )
    .await;
    assert_eq!(pax_passed(&w), 1, "PAX did pass once");
    assert!(blocked_with(&w, "completion refused"), "{}", describe(&w));
    assert!(
        !w.verified,
        "the earlier pass does not cover the final state"
    );

    // The same sequence, re-tested after the last change, is accepted.
    let dir = project("stale-ok", FAILING_TEST);
    let (w, _) = run_kind(
        &dir,
        GoalKind::Change,
        Some(&NoLocalPolicy),
        vec![
            write("src/lib.rs", RIGHT),
            pax_test(),
            write(
                "src/lib.rs",
                &format!("{RIGHT}\n// edited after the test\n"),
            ),
            pax_test(),
            complete("the tests pass"),
        ],
    )
    .await;
    assert!(
        matches!(w.report.outcome, WorkOutcome::Completed { .. }),
        "{}",
        describe(&w)
    );
    assert!(w.verified);

    // Verify work: a passing test, then a change, is no longer a statement about the project as
    // it was, so the claim is refused.
    let dir = project("stale-verify", PASSING_TEST);
    let (w, _) = run_kind(
        &dir,
        GoalKind::Verify,
        Some(&NoLocalPolicy),
        vec![
            pax_test(),
            write("src/lib.rs", RIGHT),
            complete("it passes"),
        ],
    )
    .await;
    assert_eq!(pax_passed(&w), 1);
    assert!(blocked_with(&w, "completion refused"), "{}", describe(&w));
    assert!(!w.verified);
}

/// The model cannot manufacture completion in any kind: with no evidence, "complete" is refused,
/// nothing executes as a side effect, and nothing changes.
#[tokio::test(flavor = "multi_thread")]
async fn a_completion_claim_without_evidence_is_refused_for_every_kind() {
    let dir = project("claim", FAILING_TEST);
    if !pax_available(&dir) {
        return;
    }
    let before = snapshot(&dir);
    for kind in GoalKind::ALL {
        let (w, m) = run_kind(
            &dir,
            kind,
            Some(&NoLocalPolicy),
            vec![complete("Looks good. src/lib.rs is fine.")],
        )
        .await;
        assert!(
            blocked_with(&w, "completion refused"),
            "{kind:?}: {}",
            describe(&w)
        );
        assert_eq!(started(&w), 0, "{kind:?}: nothing executed");
        assert_eq!(m.calls(), 1, "{kind:?}: no retry");
        assert!(
            !w.verified && w.exit_status() == chip_cli::verify::EXIT_NOT_VERIFIED,
            "{kind:?}"
        );
        assert_eq!(before, snapshot(&dir), "{kind:?}");
        w.audit.assert_clean();
    }
}

/// A non-mutating goal is no way around the invocation boundary: an undeclared capability is
/// refused whatever the kind.
#[tokio::test(flavor = "multi_thread")]
async fn no_kind_widens_the_invocation_boundary() {
    let dir = project("kinds-boundary", FAILING_TEST);
    if !pax_available(&dir) {
        return;
    }
    for kind in GoalKind::ALL {
        let (w, _) = run_kind(
            &dir,
            kind,
            None,
            vec![request("shell.exec", r#"{"command":"ls"}"#)],
        )
        .await;
        assert!(
            matches!(&w.report.outcome, WorkOutcome::Failed { reason } if reason.contains("unknown capability")),
            "{kind:?}: {}",
            describe(&w)
        );
        assert_eq!(started(&w), 0, "{kind:?}");
    }
}

// ---- large files ----------------------------------------------------------------------------------------

fn read_range(path: &str, offset: usize, length: usize) -> String {
    request(
        "project.read",
        &format!(
            r#"{{"path":{},"offset":{offset},"length":{length}}}"#,
            q(path)
        ),
    )
}

/// A file larger than 32 KiB, inspected through bounded ranges. The fixture is real source (this
/// repository's own work loop, over 100 KB). Range edges are still placed on character boundaries,
/// as a caller following a `range_splits_character` refusal would (that refusal itself is tested in
/// `chip-project`).
#[tokio::test(flavor = "multi_thread")]
async fn a_file_larger_than_the_read_limit_is_inspectable_through_bounded_observations() {
    let dir = project("large", FAILING_TEST);
    if !pax_available(&dir) {
        return;
    }
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../chip-core/src/work.rs"),
    )
    .unwrap();
    const MAX: usize = 32 * 1024;
    assert!(
        source.len() > 3 * MAX,
        "the fixture must be larger than three reads"
    );
    std::fs::write(dir.join("src/big.rs"), &source).unwrap();

    // The nearest character boundary at or before a byte position.
    let boundary = |mut at: usize| {
        while !source.is_char_boundary(at) {
            at -= 1;
        }
        at
    };
    // Something only a later part of the file contains.
    let marker = "fn answer_refused";
    let at = source.find(marker).expect("a marker in the source");
    assert!(at > MAX, "the marker lies beyond the first read: {at}");

    let (b1, b2) = (boundary(MAX), boundary(2 * MAX));
    let (c0, c1, c2) = (0, b1, b2);
    let middle_start = boundary(at - 200);
    let final_start = boundary(source.len() - 1000);
    let (w, m) = run_kind(
        &dir,
        GoalKind::Inspect,
        None,
        vec![
            read("src/big.rs"),
            read_range("src/big.rs", c0, b1 - c0),
            read_range("src/big.rs", middle_start, 600),
            read_range("src/big.rs", final_start, MAX),
            complete("src/big.rs defines `answer_refused`, in the part of the file past the first 32 KiB."),
        ],
    )
    .await;
    let _ = (c1, c2);

    let outputs: Vec<&str> = w
        .report
        .observations
        .iter()
        .filter_map(|o| o.output.as_deref())
        .collect();
    assert_eq!(outputs.len(), 4);
    // A whole-file read is still refused; the limit did not move.
    assert!(
        outputs[0].contains("\"error\":\"too_large\""),
        "{}",
        &outputs[0][..outputs[0].len().min(200)]
    );
    // Each range is bounded and is exactly the bytes asked for.
    let body = |o: &str| o.split("--- content ---\n").nth(1).unwrap().to_string();
    let (first, middle, last) = (body(outputs[1]), body(outputs[2]), body(outputs[3]));
    for part in [&first, &middle, &last] {
        assert!(
            part.len() <= MAX,
            "no single observation exceeds the read limit"
        );
    }
    assert_eq!(first, source[..b1]);
    assert_eq!(middle, source[middle_start..middle_start + 600]);
    assert_eq!(last, source[final_start..]);
    // They are distinct portions of the file, and the model saw the later ones.
    assert!(first != middle && middle != last && first != last);
    assert!(
        !first.contains(marker) && middle.contains(marker),
        "the marker is only in the later range"
    );
    assert!(
        m.request(3).contains(marker),
        "the model was shown the later range"
    );
    assert!(
        !m.request(2).contains(marker),
        "and had not seen it before asking"
    );
    // The head of each observation says what part of what it is.
    let head: serde_json::Value = serde_json::from_str(outputs[2].lines().next().unwrap()).unwrap();
    assert_eq!(head["offset"], middle_start);
    assert_eq!(head["file_bytes"], source.len());
    assert_eq!(head["complete"], false);
    // The answer cites the file; reading it in parts grounds it exactly as a whole read would.
    assert!(
        matches!(w.report.outcome, WorkOutcome::Completed { .. }),
        "{}",
        describe(&w)
    );
    assert!(w.grounded && !w.verified, "grounded is not verified");
    assert_eq!(w.exit_status(), chip_cli::verify::EXIT_NOT_VERIFIED);
    assert_eq!(
        std::fs::read_to_string(dir.join("src/big.rs")).unwrap(),
        source,
        "nothing changed"
    );
    w.audit.assert_clean();
}
