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
use chip_cli::software_work::{CompleteWhenVerified, SoftwareWork, run_software_work_with_budget};
use chip_core::{
    EnvironmentDescription, ExecutionEvent, ObservationKind, WorkEvent, WorkId, WorkLimits,
    WorkOutcome,
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

/// Scenario 1. Inspecting works. Reporting what was found does not complete the work: the product's
/// goal is "a file changed and PAX passed after it", so a claim of completion is refused, and a
/// finding can only be delivered as the reason for a block. (An audit finding: see capabilities.md.)
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

/// Scenario 5. A goal that already holds cannot be completed: completion requires a content-changing
/// write followed by a passing test, so the honest no-op ends as a refused claim. (An audit finding.)
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
