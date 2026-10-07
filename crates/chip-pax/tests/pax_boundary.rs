//! PR43: Chip consumes PAX's `pax.execution-result.v1` as an authoritative observation.
//!
//! Wherever PAX itself can produce a case (passed, failed, compilation failed, zero tests,
//! ignored-only, unsupported, ambiguous, a launch error) the real installed PAX 0.3.0 and the real
//! native tools run against real throwaway projects. A small shim stands in at the process
//! boundary in two kinds of test only, and says so: (1) output a real PAX would never emit (the
//! malformed-result cases), and (2) other PAX versions (the version gate). It never replaces PAX
//! in a test that claims a real execution.
//!
//! Only the *model* is scripted (a fixture that replies with fixed text through the real strict
//! parser). If PAX is not installed a test says SKIPPED and does nothing.
//!
//! Chip learns nothing about Cargo, npm or any test framework here. Where these tests mention a
//! tool they assert what PAX said, never what the tool printed.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chip_core::{
    Agent, CapabilityAvailability, CapabilityId, CapabilityProvider, ExecutionError,
    ExecutionEvent, ExecutionId, ExecutionObserver, ExecutionRequest, ExecutionStatus, Executor,
    LocalWorkPolicy, ModelDecisionBoundary, Observation, ObservationKind, ObservationPredicate,
    TestLocalReasoner, WorkDecision, WorkEvent, WorkGoal, WorkId, WorkLimits, WorkOutcome,
    WorkReport, WorkSpec, WorkView, audit_safety, measure_utility, verify_trajectory,
};
use chip_pax::{
    PAX_TEST_CAPABILITY, PaxExecutionResult, PaxExecutor, PaxResultError, PaxStatus, PaxTestPassed,
    PaxUnavailable, RESULT_SCHEMA, parse_execution_result, render_observation,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

// ---- fixtures ---------------------------------------------------------------------------------

struct Script {
    replies: Mutex<VecDeque<String>>,
}

#[async_trait::async_trait]
impl ModelProvider for Script {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse, FxError> {
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| FxError::Provider("unscripted model call".into()))?;
        Ok(ModelResponse::new("msg1", reply, Usage::new(0, 0)))
    }
}

/// Ask the model first; after the observation propose to complete if it completed, block if it
/// failed. The loop, not this policy, decides whether a completion is allowed.
struct ReactToObservation;

impl LocalWorkPolicy for ReactToObservation {
    fn propose(&self, view: &WorkView<'_>) -> Option<WorkDecision> {
        if view.turn == 0 {
            return None;
        }
        Some(match view.observations.last().map(|o| o.kind) {
            Some(ObservationKind::ExecutionCompleted) => WorkDecision::Complete {
                summary: "pax.test established the goal".into(),
            },
            Some(ObservationKind::ExecutionFailed) => WorkDecision::Block {
                reason: "pax.test did not establish the goal".into(),
            },
            _ => WorkDecision::Escalate {
                reason: "no usable observation".into(),
            },
        })
    }
}

/// Always ask the model: for tests of what the model may claim.
struct AskAgain;

impl LocalWorkPolicy for AskAgain {
    fn propose(&self, _view: &WorkView<'_>) -> Option<WorkDecision> {
        None
    }
}

fn unique(tag: &str) -> PathBuf {
    prune_stale_fixtures();
    let dir = std::env::temp_dir().join(format!("chip-pax-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Removes fixtures that earlier runs left behind (other processes, over ten minutes old).
fn prune_stale_fixtures() {
    let me = format!("chip-pax-{}-", std::process::id());
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|age| age.as_secs() > 600));
        if name.starts_with("chip-pax-") && !name.starts_with(&me) && stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// A throwaway Rust project with the given `src/lib.rs`.
fn rust_project(tag: &str, lib: &str) -> PathBuf {
    let dir = unique(tag);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let name = tag.replace('-', "_");
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"chipfixture_{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("src/lib.rs"), lib).unwrap();
    dir
}

fn with_test(tag: &str, attrs: &str, body: &str) -> PathBuf {
    rust_project(
        tag,
        &format!(
            "#[cfg(test)]\nmod tests {{\n    #[test]\n    {attrs}\n    fn the_test() {{\n        {body}\n    }}\n}}\n"
        ),
    )
}

fn passing(tag: &str) -> PathBuf {
    with_test(tag, "", "assert_eq!(2 + 2, 4);")
}

fn failing(tag: &str) -> PathBuf {
    with_test(tag, "", "assert_eq!(2 + 2, 5);")
}

/// No test at all: the native process exits 0, and PAX says no tests were executed.
fn zero_tests(tag: &str) -> PathBuf {
    rust_project(tag, "pub fn nothing_to_test() -> u32 { 1 }\n")
}

/// The only test is ignored: zero executed, one ignored.
fn ignored_only(tag: &str) -> PathBuf {
    with_test(tag, "#[ignore]", "assert!(false);")
}

fn real_pax(dir: &Path) -> Option<PaxExecutor> {
    let pax = PaxExecutor::new(dir);
    match tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(pax.resolve()))
    {
        Ok(_) => Some(pax),
        Err(why) => {
            eprintln!("SKIPPED: {why}");
            None
        }
    }
}

fn request(capability: &str) -> String {
    format!(
        r#"{{"schema":"chip.work-decision.v1","decision":"request_capability","capability":"{capability}"}}"#
    )
}

fn with_fields(extra: &str) -> String {
    format!(
        r#"{{"schema":"chip.work-decision.v1","decision":"request_capability","capability":"pax.test",{extra}}}"#
    )
}

fn complete(summary: &str) -> String {
    format!(r#"{{"decision":"complete","summary":"{summary}"}}"#)
}

fn spec() -> WorkSpec {
    WorkSpec::new(
        WorkId::new("pax"),
        WorkGoal::new("Verify that the project's tests pass."),
    )
    .with_limits(WorkLimits {
        max_turns: 4,
        max_executions: 2,
    })
    .with_required_observation(Arc::new(PaxTestPassed))
}

async fn run(
    pax: PaxExecutor,
    replies: &[String],
    policy: &dyn LocalWorkPolicy,
) -> (WorkReport, WorkSpec) {
    let model = Arc::new(Script {
        replies: Mutex::new(replies.iter().cloned().collect()),
    });
    let agent = Agent::new(model)
        .with_capabilities(Arc::new(pax.clone()))
        .with_executor(Arc::new(pax))
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(Arc::new(TestLocalReasoner::default()));
    let spec = spec();
    let report = agent.run_work(&spec, policy, &ModelDecisionBoundary).await;
    (report, spec)
}

fn declared() -> Vec<CapabilityId> {
    vec![CapabilityId::new(PAX_TEST_CAPABILITY).unwrap()]
}

fn executions(r: &WorkReport) -> usize {
    r.events
        .iter()
        .filter(|e| {
            matches!(
                e,
                WorkEvent::Execution(ExecutionEvent::ExecutionStarted { .. })
            )
        })
        .count()
}

fn evidence(r: &WorkReport) -> usize {
    r.events
        .iter()
        .filter(|e| matches!(e, WorkEvent::EvidenceRecorded { .. }))
        .count()
}

fn completed(r: &WorkReport) -> bool {
    matches!(r.outcome, WorkOutcome::Completed { .. })
}

fn satisfied_events(r: &WorkReport) -> Vec<bool> {
    r.events
        .iter()
        .filter_map(|e| match e {
            WorkEvent::GoalEvaluated { satisfied, .. } => Some(*satisfied),
            _ => None,
        })
        .collect()
}

fn clean(report: &WorkReport, spec: &WorkSpec) {
    audit_safety(report, spec, &declared()).assert_clean();
    let violations = verify_trajectory(&report.events, &spec.limits);
    assert!(violations.is_empty(), "{violations:?}");
}

fn observation_text(report: &WorkReport) -> &str {
    report
        .observations
        .last()
        .and_then(|o| o.output.as_deref())
        .expect("an observation")
}

/// What PAX said, read back from the first line of the recorded observation.
fn pax_said(report: &WorkReport) -> PaxExecutionResult {
    let first = observation_text(report).lines().next().unwrap();
    parse_execution_result(first.as_bytes()).expect("a canonical result on line one")
}

/// A candidate executable that is a script (unix).
#[cfg(unix)]
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A stand-in at the process boundary: answers `--version` as `pax <version>`; any other call is
/// logged and answered with the given stdout bytes, stderr bytes and exit status. For the
/// malformed-result and version-gate tests only.
#[cfg(unix)]
struct Shim {
    path: PathBuf,
    log: PathBuf,
}

#[cfg(unix)]
fn shim(tag: &str, version: &str, stdout: &[u8], stderr: &[u8], exit: i32) -> Shim {
    let dir = unique(&format!("shim-{tag}"));
    std::fs::write(dir.join("out.bin"), stdout).unwrap();
    std::fs::write(dir.join("err.bin"), stderr).unwrap();
    let log = dir.join("calls.log");
    let path = script(
        &dir,
        "pax",
        &format!(
            "if [ \"$1\" = \"--version\" ]; then echo 'pax {version}'; exit 0; fi\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> '{log}'; done; printf -- '--\\n' >> '{log}'\ncat '{out}'\ncat '{err}' >&2\nexit {exit}",
            log = log.display(),
            out = dir.join("out.bin").display(),
            err = dir.join("err.bin").display(),
        ),
    );
    Shim { path, log }
}

#[cfg(unix)]
impl Shim {
    /// The calls made besides `--version`.
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .split("--\n")
            .filter(|c| !c.is_empty())
            .map(str::to_string)
            .collect()
    }
}

fn result_json(status: &str, reason: &str, tool: Option<&str>, exit: Option<i64>) -> String {
    format!(
        r#"{{"schema":"{RESULT_SCHEMA}","operation":"test","status":"{status}","reason":"{reason}","tool":{},"exit_code":{}}}"#,
        tool.map_or("null".to_string(), |t| format!("\"{t}\"")),
        exit.map_or("null".to_string(), |c| c.to_string()),
    )
}

// ---- the capability ----------------------------------------------------------------------------------

#[tokio::test]
async fn pax_test_is_declared_with_no_inputs_and_describes_no_ecosystem() {
    let dir = unique("declared");
    let pax = PaxExecutor::new(&dir);
    let found = pax.capabilities().await.unwrap();
    assert_eq!(found.len(), 1, "one capability, no generic surface");
    assert_eq!(found[0].id.as_str(), "pax.test");
    assert!(found[0].inputs.is_empty(), "nothing for a model to fill in");
    assert!(
        !found[0].reuse_evidence,
        "a test result must never answer a later request for the project's tests"
    );
    // The model learns that the project can be tested, not how any tool does it.
    let shown =
        format!("{} {} {}", found[0].id, found[0].name, found[0].description).to_lowercase();
    for word in [
        "cargo", "npm", "pytest", "pnpm", "yarn", "bun", "rust", "python", "docker", "libtest",
        "exit",
    ] {
        assert!(!shown.contains(word), "the capability mentions {word}");
    }
    // The invocation is fixed, and holds no model-controlled value.
    assert_eq!(
        pax.invocation(),
        [
            std::ffi::OsString::from("--dir"),
            dir.clone().into_os_string(),
            std::ffi::OsString::from("--json"),
            std::ffi::OsString::from("test"),
        ]
    );
}

// ---- nothing invalid reaches PAX -----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn no_invalid_invocation_or_model_authored_result_reaches_pax() {
    let invalid: Vec<(&str, String)> = vec![
        ("empty inputs", with_fields(r#""inputs":{}"#)),
        (
            "input: command",
            with_fields(r#""inputs":{"command":"cargo test"}"#),
        ),
        (
            "input: executable",
            with_fields(r#""inputs":{"executable":"/bin/sh"}"#),
        ),
        ("input: args", with_fields(r#""inputs":{"args":"--help"}"#)),
        (
            "input: directory",
            with_fields(r#""inputs":{"directory":"/"}"#),
        ),
        (
            "input: status",
            with_fields(r#""inputs":{"status":"passed"}"#),
        ),
        ("field: executable", with_fields(r#""executable":"cargo""#)),
        ("field: command", with_fields(r#""command":"pax test""#)),
        ("field: args", with_fields(r#""args":["test"]"#)),
        (
            "field: working_directory",
            with_fields(r#""working_directory":"/tmp""#),
        ),
        ("field: cwd", with_fields(r#""cwd":"/""#)),
        ("field: project_root", with_fields(r#""project_root":"/""#)),
        ("field: env", with_fields(r#""env":{"A":"1"}"#)),
        // A model supplying what only a real PAX run can produce.
        ("field: status", with_fields(r#""status":"passed""#)),
        ("field: reason", with_fields(r#""reason":"tests-passed""#)),
        ("field: tool", with_fields(r#""tool":"cargo""#)),
        ("field: exit_code", with_fields(r#""exit_code":0"#)),
        (
            "field: tests",
            with_fields(r#""tests":{"passed":1,"failed":0,"ignored":0,"measured":0}"#),
        ),
        ("field: result", with_fields(r#""result":"tests passed""#)),
        (
            "field: observation",
            with_fields(r#""observation":"it passed""#),
        ),
        ("field: evidence", with_fields(r#""evidence":"recorded""#)),
        (
            "field: receipt",
            with_fields(r#""receipt":"sha256:forged""#),
        ),
        (
            "field: execution_id",
            with_fields(r#""execution_id":"mine""#),
        ),
        (
            "a whole result in the decision",
            with_fields(&format!(r#""result_schema":"{RESULT_SCHEMA}""#)),
        ),
        ("undeclared: pax.build", request("pax.build")),
        ("undeclared: pax.exec", request("pax.exec")),
        ("undeclared: bare pax", request("pax")),
        ("undeclared: cargo.test", request("cargo.test")),
        ("prose", "I ran the tests and they passed.".to_string()),
        (
            "a result as the reply",
            result_json("passed", "tests-passed", Some("cargo"), Some(0)),
        ),
    ];
    // Real PAX over a real project: if PAX ran, Cargo would create `target/` here.
    let dir = passing("invalid");
    let Some(pax) = real_pax(&dir) else { return };
    for (what, reply) in invalid {
        let (report, spec) = run(pax.clone(), &[reply], &ReactToObservation).await;
        assert_eq!(executions(&report), 0, "{what}: reached PAX");
        assert!(report.observations.is_empty(), "{what}");
        assert_eq!(evidence(&report), 0, "{what}");
        assert!(
            satisfied_events(&report).is_empty(),
            "{what}: something was evaluated"
        );
        assert!(!completed(&report), "{what}");
        assert!(
            !dir.join("target").exists(),
            "{what}: the native tool ran, so PAX was reached"
        );
        clean(&report, &spec);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_executor_asked_for_anything_else_refuses() {
    let dir = passing("other-intent");
    let Some(pax) = real_pax(&dir) else { return };
    for intent in ["pax.build", "pax", "pax test", "sh", ""] {
        let outcome = pax
            .execute(ExecutionRequest::new(ExecutionId::new("x"), intent))
            .await;
        assert!(
            matches!(outcome, Err(ExecutionError::InvalidRequest(_))),
            "{intent:?}: {outcome:?}"
        );
    }
    // `pax.test` takes no inputs, so an input that reaches the executor is refused whatever it is.
    for (name, value) in [
        ("command", chip_core::InputValue::Text("cargo test".into())),
        ("status", chip_core::InputValue::Text("passed".into())),
        ("anything", chip_core::InputValue::Bool(true)),
    ] {
        let request = ExecutionRequest::new(ExecutionId::new("x"), "pax.test")
            .with_inputs([(name.to_string(), value)].into());
        let outcome = pax.execute(request).await;
        assert!(
            matches!(outcome, Err(ExecutionError::InvalidRequest(_))),
            "{name}: {outcome:?}"
        );
    }
    assert!(!dir.join("target").exists());
}

// ---- missing, wrong and old PAX: fail closed ----------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn missing_pax_fails_closed_and_fabricates_nothing() {
    let dir = passing("missing");
    let pax = PaxExecutor::new(&dir).with_search_path(unique("empty-path").into_os_string());
    assert!(matches!(
        pax.resolve().await,
        Err(PaxUnavailable::NotFound(_))
    ));
    let id = CapabilityId::new("pax.test").unwrap();
    assert!(matches!(
        pax.availability(&id).await,
        CapabilityAvailability::Unavailable(_)
    ));
    let outcome = pax
        .execute(ExecutionRequest::new(ExecutionId::new("x"), "pax.test"))
        .await;
    assert!(
        matches!(outcome, Err(ExecutionError::ExecutorUnavailable(_))),
        "{outcome:?}"
    );
    let (report, spec) = run(pax, &[request("pax.test")], &ReactToObservation).await;
    assert_eq!(
        (
            executions(&report),
            report.observations.len(),
            evidence(&report)
        ),
        (0, 0, 0)
    );
    assert!(!completed(&report));
    clean(&report, &spec);
    assert!(!dir.join("target").exists());
}

#[tokio::test]
async fn a_missing_work_directory_fails_closed() {
    let pax = PaxExecutor::new(std::env::temp_dir().join("chip-pax-no-such-directory-xyz"));
    assert!(matches!(
        pax.resolve().await,
        Err(PaxUnavailable::BadWorkDirectory(_))
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn the_posix_archive_utility_is_not_pax() {
    let posix = Path::new("/bin/pax");
    if !posix.is_file() {
        eprintln!("SKIPPED: no /bin/pax on this machine");
        return;
    }
    let dir = passing("posix");
    let pax = PaxExecutor::new(&dir).with_binary(posix);
    match pax.resolve().await {
        Err(PaxUnavailable::NotPax { path, .. }) => assert_eq!(path, posix),
        other => panic!("the POSIX archive utility was accepted as PAX: {other:?}"),
    }
    let outcome = pax
        .execute(ExecutionRequest::new(ExecutionId::new("x"), "pax.test"))
        .await;
    assert!(matches!(
        outcome,
        Err(ExecutionError::ExecutorUnavailable(_))
    ));
    assert!(!dir.join("target").exists());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn lookup_takes_the_first_pax_and_never_falls_back_to_another() {
    let real_dir = unique("lookup-real");
    let Some(real) = real_pax(&real_dir) else {
        return;
    };
    if !Path::new("/bin/pax").is_file() {
        eprintln!("SKIPPED: no /bin/pax on this machine");
        return;
    }
    let real_path = real.resolve().await.unwrap().path;
    let real_bin_dir = real_path.parent().unwrap().to_path_buf();
    let dir = passing("lookup");
    let search = std::env::join_paths([PathBuf::from("/bin"), real_bin_dir.clone()]).unwrap();
    match PaxExecutor::new(&dir)
        .with_search_path(search)
        .resolve()
        .await
    {
        Err(PaxUnavailable::NotPax { path, .. }) => assert_eq!(path, Path::new("/bin/pax")),
        other => panic!("expected the first candidate to be refused, got {other:?}"),
    }
    let search = std::env::join_paths([real_bin_dir, PathBuf::from("/bin")]).unwrap();
    let found = PaxExecutor::new(&dir)
        .with_search_path(search)
        .resolve()
        .await
        .unwrap();
    assert_eq!(found.path, real_path);
    assert!(!dir.join("target").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn anything_that_does_not_say_it_is_pax_is_refused() {
    let scripts = unique("impostors");
    let dir = passing("impostor");
    for (name, body) in [
        ("exit-nonzero", "echo 'pax 0.3.0'; exit 1"),
        ("not-pax", "echo 'something else 1.0.0'"),
        ("two-lines", "echo 'pax 0.3.0'; echo 'more'"),
        ("no-version", "echo 'pax'"),
        ("bad-version", "echo 'pax banana'"),
        ("two-part", "echo 'pax 0.3'"),
        ("prefix-only", "echo 'xpax 0.3.0'"),
        ("silent", "true"),
    ] {
        let path = script(&scripts, name, body);
        assert!(
            matches!(
                PaxExecutor::new(&dir).with_binary(&path).resolve().await,
                Err(PaxUnavailable::NotPax { .. })
            ),
            "{name} was accepted as PAX"
        );
    }
    assert!(!dir.join("target").exists());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn an_older_pax_is_unavailable_and_nothing_native_runs() {
    for old in ["0.2.0", "0.2.99", "0.2.10", "0.1.0", "0.3.0-rc.1", "0.0.1"] {
        let dir = passing("old");
        let s = shim(
            &format!("old-{old}"),
            old,
            result_json("passed", "tests-passed", Some("cargo"), Some(0)).as_bytes(),
            b"",
            0,
        );
        let pax = PaxExecutor::new(&dir).with_binary(&s.path);
        match pax.resolve().await {
            Err(PaxUnavailable::TooOld {
                found, required, ..
            }) => {
                assert_eq!((found.as_str(), required.as_str()), (old, "0.3.0"));
            }
            other => panic!("PAX {old} was accepted: {other:?}"),
        }
        let id = CapabilityId::new("pax.test").unwrap();
        assert!(
            matches!(
                pax.availability(&id).await,
                CapabilityAvailability::Unavailable(_)
            ),
            "{old}"
        );
        // Through the loop: the capability is not offered, so a request for it executes nothing.
        let (report, spec) = run(pax, &[request("pax.test")], &ReactToObservation).await;
        assert_eq!(
            (
                executions(&report),
                report.observations.len(),
                evidence(&report)
            ),
            (0, 0, 0),
            "{old}"
        );
        assert!(!completed(&report), "{old}");
        clean(&report, &spec);
        // PAX was asked its version and nothing else: no project command, native or otherwise.
        assert!(s.calls().is_empty(), "{old}: PAX was run: {:?}", s.calls());
        assert!(!dir.join("target").exists(), "{old}");
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_later_pax_is_accepted_and_its_result_consumed() {
    for later in [
        "0.3.0",
        "0.3.1",
        "0.4.0",
        "0.10.0",
        "1.0.0",
        "0.3.0+build.7",
    ] {
        let dir = passing("later");
        let stdout = result_json("passed", "tests-passed", Some("cargo"), Some(0));
        let s = shim(
            &format!("later-{later}"),
            later,
            stdout.as_bytes(),
            b"native diagnostics\n",
            0,
        );
        let pax = PaxExecutor::new(&dir).with_binary(&s.path);
        assert_eq!(pax.resolve().await.unwrap().version, later);
        let (report, spec) = run(pax, &[request("pax.test")], &ReactToObservation).await;
        clean(&report, &spec);
        assert!(completed(&report), "{later}: {:?}", report.outcome);
        assert_eq!(s.calls().len(), 1, "{later}");
    }
}

// ---- real PAX, real native tools: every status --------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_passing_project_completes_because_pax_established_passed() {
    let dir = passing("real-pass");
    let Some(pax) = real_pax(&dir) else { return };
    let (report, spec) = run(pax, &[request("pax.test")], &ReactToObservation).await;
    clean(&report, &spec);
    let said = pax_said(&report);
    assert_eq!(
        (
            said.status,
            said.reason.as_str(),
            said.tool.as_deref(),
            said.exit_code
        ),
        (PaxStatus::Passed, "tests-passed", Some("cargo"), Some(0))
    );
    assert!(said.tests.is_some_and(|t| t.passed == 1 && t.failed == 0));
    let o = &report.observations[0];
    assert_eq!(
        (o.kind, o.status, o.receipt_id.as_deref()),
        (
            ObservationKind::ExecutionCompleted,
            ExecutionStatus::Success,
            None
        ),
        "no receipt: PAX issues none"
    );
    assert_eq!(satisfied_events(&report), [true]);
    assert!(completed(&report), "{:?}", report.outcome);
    let names: Vec<&str> = report
        .events
        .iter()
        .filter_map(|e| match e {
            WorkEvent::ModelCalled { .. } => Some("ModelCalled"),
            WorkEvent::CapabilityRequested { .. } => Some("CapabilityRequested"),
            WorkEvent::Execution(ExecutionEvent::ExecutionStarted { .. }) => {
                Some("ExecutionStarted")
            }
            WorkEvent::ObservationRecorded { .. } => Some("ObservationRecorded"),
            WorkEvent::EvidenceRecorded { .. } => Some("EvidenceRecorded"),
            WorkEvent::GoalEvaluated {
                satisfied: true, ..
            } => Some("GoalEvaluated(true)"),
            WorkEvent::WorkCompleted { .. } => Some("WorkCompleted"),
            _ => None,
        })
        .collect();
    assert_eq!(
        names,
        [
            "ModelCalled",
            "CapabilityRequested",
            "ExecutionStarted",
            "ObservationRecorded",
            "EvidenceRecorded",
            "GoalEvaluated(true)",
            "WorkCompleted"
        ]
    );
    assert_eq!(evidence(&report), 1);
    let u = measure_utility(&report, &spec);
    assert_eq!(
        (u.required_outputs, u.verified_outputs, u.completed),
        (1, 1, true)
    );
    assert!((u.goal_coverage - 1.0).abs() < 1e-9);
    assert!(dir.join("target").exists(), "the native tool really ran");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_project_is_a_failed_observation_and_does_not_complete() {
    let dir = failing("real-fail");
    let Some(pax) = real_pax(&dir) else { return };
    let (report, spec) = run(pax, &[request("pax.test")], &ReactToObservation).await;
    clean(&report, &spec);
    let said = pax_said(&report);
    assert_eq!(
        (
            said.status,
            said.reason.as_str(),
            said.tool.as_deref(),
            said.exit_code
        ),
        (PaxStatus::Failed, "tests-failed", Some("cargo"), Some(101))
    );
    assert!(said.tests.is_some_and(|t| t.failed == 1));
    assert_eq!(
        report.observations[0].kind,
        ObservationKind::ExecutionFailed
    );
    assert_eq!(report.observations[0].receipt_id, None);
    assert_eq!(satisfied_events(&report), [false]);
    assert!(!completed(&report), "{:?}", report.outcome);
    assert_eq!((executions(&report), evidence(&report)), (1, 1));
    let u = measure_utility(&report, &spec);
    assert_eq!((u.verified_outputs, u.completed), (0, false));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_that_does_not_compile_is_failed_not_error() {
    let dir = rust_project(
        "real-compile",
        "pub fn f() -> u32 { \"x\" }\n#[cfg(test)]\nmod t { #[test] fn a() {} }\n",
    );
    let Some(pax) = real_pax(&dir) else { return };
    let (report, spec) = run(pax, &[request("pax.test")], &ReactToObservation).await;
    clean(&report, &spec);
    let said = pax_said(&report);
    assert_eq!(
        (said.status, said.reason.as_str()),
        (PaxStatus::Failed, "compilation-failed")
    );
    assert_eq!(satisfied_events(&report), [false]);
    assert!(!completed(&report));
}

#[tokio::test(flavor = "multi_thread")]
async fn zero_tests_is_not_run_with_exit_code_zero_and_does_not_satisfy_the_goal() {
    // The mandatory regression: the native process exited 0, and PAX says no tests ran.
    for (tag, dir) in [
        ("real-zero", zero_tests("real-zero")),
        ("real-ignored", ignored_only("real-ignored")),
    ] {
        let Some(pax) = real_pax(&dir) else { return };
        let (report, spec) = run(pax.clone(), &[request("pax.test")], &ReactToObservation).await;
        clean(&report, &spec);
        let said = pax_said(&report);
        assert_eq!(said.status, PaxStatus::NotRun, "{tag}");
        assert_eq!(said.reason, "no-tests-executed", "{tag}");
        assert_eq!(
            said.exit_code,
            Some(0),
            "{tag}: the exit code alone would have looked like success"
        );
        // Not a successful execution either: only `passed` is.
        assert_eq!(
            report.observations[0].kind,
            ObservationKind::ExecutionFailed,
            "{tag}"
        );
        assert_eq!(
            report.observations[0].status,
            ExecutionStatus::Failure,
            "{tag}"
        );
        assert_eq!(satisfied_events(&report), [false], "{tag}");
        assert!(!completed(&report), "{tag}: {:?}", report.outcome);
        assert_eq!(measure_utility(&report, &spec).verified_outputs, 0, "{tag}");
        // And a model that insists the work is done is refused.
        let (report, spec) = run(
            pax,
            &[request("pax.test"), complete("All tests passed.")],
            &AskAgain,
        )
        .await;
        clean(&report, &spec);
        assert!(!completed(&report), "{tag}");
        assert!(
            matches!(&report.outcome, WorkOutcome::Blocked { reason } if reason.starts_with("completion refused")),
            "{tag}: {:?}",
            report.outcome
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unsupported_tool_is_unsupported_even_though_its_exit_code_is_zero() {
    // An npm project whose test script exits 0. PAX runs it and preserves the exit code, and says
    // it has no authoritative test semantics for npm. Exit 0 must not be mistaken for a pass.
    let dir = unique("real-npm");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"npmfixture","version":"1.0.0","scripts":{"test":"touch ran.marker"}}"#,
    )
    .unwrap();
    std::fs::write(dir.join("package-lock.json"), "").unwrap();
    let Some(pax) = real_pax(&dir) else { return };
    let (report, spec) = run(pax, &[request("pax.test")], &ReactToObservation).await;
    clean(&report, &spec);
    if report.observations.is_empty() {
        eprintln!(
            "SKIPPED: no observation (npm unavailable?): {:?}",
            report.outcome
        );
        return;
    }
    let said = pax_said(&report);
    if said.status == PaxStatus::Error {
        eprintln!("SKIPPED: npm could not be launched here");
        return;
    }
    assert_eq!(
        (
            said.status,
            said.reason.as_str(),
            said.tool.as_deref(),
            said.exit_code
        ),
        (
            PaxStatus::Unsupported,
            "interpretation-unsupported",
            Some("npm"),
            Some(0)
        )
    );
    assert!(
        dir.join("ran.marker").exists(),
        "PAX did run the script: the exit code really was 0"
    );
    assert_eq!(satisfied_events(&report), [false]);
    assert!(!completed(&report));
    assert_eq!(measure_utility(&report, &spec).verified_outputs, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_operation_pax_does_not_support_executes_nothing() {
    let dir = unique("real-noscript");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"ns","version":"1.0.0"}"#,
    )
    .unwrap();
    std::fs::write(dir.join("package-lock.json"), "").unwrap();
    let Some(pax) = real_pax(&dir) else { return };
    let (report, spec) = run(pax, &[request("pax.test")], &ReactToObservation).await;
    clean(&report, &spec);
    let said = pax_said(&report);
    assert_eq!(
        (said.status, said.reason.as_str(), said.tool, said.exit_code),
        (PaxStatus::Unsupported, "operation-unsupported", None, None)
    );
    assert_eq!(satisfied_events(&report), [false]);
    assert!(!completed(&report));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ambiguous_project_is_ambiguous_and_no_native_command_runs() {
    let dir = unique("real-ambiguous");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"amb","version":"1.0.0","scripts":{"test":"touch ran.marker"}}"#,
    )
    .unwrap();
    std::fs::write(dir.join("package-lock.json"), "").unwrap();
    std::fs::write(dir.join("pnpm-lock.yaml"), "").unwrap();
    let Some(pax) = real_pax(&dir) else { return };
    let (report, spec) = run(pax, &[request("pax.test")], &ReactToObservation).await;
    clean(&report, &spec);
    let said = pax_said(&report);
    assert_eq!(
        (said.status, said.reason.as_str(), said.tool, said.exit_code),
        (PaxStatus::Ambiguous, "ambiguous-selection", None, None)
    );
    assert!(
        !dir.join("ran.marker").exists(),
        "a native test command ran for an ambiguous project"
    );
    assert_eq!(satisfied_events(&report), [false]);
    assert!(!completed(&report));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_pax_execution_error_is_an_error_not_a_failed_test_suite() {
    // The real PAX, run with a PATH that has no Cargo on it: it cannot launch the native tool and
    // reports so. Only the environment is restricted; PAX is the real one.
    let dir = passing("real-error");
    let Some(real) = real_pax(&dir) else { return };
    let real_path = real.resolve().await.unwrap().path;
    let wrapper = script(
        &unique("error-wrapper"),
        "pax",
        &format!("PATH=/usr/bin:/bin exec '{}' \"$@\"", real_path.display()),
    );
    let pax = PaxExecutor::new(&dir).with_binary(&wrapper);
    let (report, spec) = run(pax, &[request("pax.test")], &ReactToObservation).await;
    clean(&report, &spec);
    let said = pax_said(&report);
    assert_eq!(
        (said.status, said.reason.as_str(), said.exit_code),
        (PaxStatus::Error, "launch-failed", None)
    );
    assert_ne!(
        said.status,
        PaxStatus::Failed,
        "a launch error is not a failing suite"
    );
    assert_eq!(
        report.observations[0].kind,
        ObservationKind::ExecutionFailed
    );
    assert_eq!(
        report.observations[0].receipt_id, None,
        "no receipt, no successful evidence"
    );
    assert_eq!(satisfied_events(&report), [false]);
    assert!(!completed(&report));
    assert!(!dir.join("target").exists(), "Cargo never ran");
    assert_eq!(measure_utility(&report, &spec).verified_outputs, 0);
}

// ---- a model's claims are not reality ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_completion_claim_cannot_override_what_pax_established() {
    for (tag, dir) in [
        ("claim-fail (failed)", failing("claim-fail")),
        (
            "claim-zero (not_run, exit code 0)",
            zero_tests("claim-zero"),
        ),
    ] {
        let Some(pax) = real_pax(&dir) else { return };
        let (report, spec) = run(
            pax,
            &[request("pax.test"), complete("All tests passed.")],
            &AskAgain,
        )
        .await;
        clean(&report, &spec);
        assert!(!completed(&report), "{tag}: a claim completed the work");
        assert!(
            matches!(&report.outcome, WorkOutcome::Blocked { reason } if reason.starts_with("completion refused")),
            "{tag}: {:?}",
            report.outcome
        );
        assert_eq!((executions(&report), report.observations.len()), (1, 1));
        assert_eq!(satisfied_events(&report), [false]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_completion_claim_before_any_observation_is_refused() {
    let dir = passing("claim-early");
    let Some(pax) = real_pax(&dir) else { return };
    let (report, spec) = run(pax, &[complete("The tests passed.")], &AskAgain).await;
    clean(&report, &spec);
    assert!(!completed(&report));
    assert_eq!(
        (
            executions(&report),
            report.observations.len(),
            evidence(&report)
        ),
        (0, 0, 0)
    );
    assert!(!dir.join("target").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn model_prose_is_never_evidence() {
    let dir = failing("prose");
    let Some(pax) = real_pax(&dir) else { return };
    for prose in [
        "The tests passed.",
        r#"{"status":"passed"}"#,
        "Result: 3 passed, 0 failed",
    ] {
        let (report, spec) = run(pax.clone(), &[prose.to_string()], &ReactToObservation).await;
        clean(&report, &spec);
        assert_eq!((executions(&report), evidence(&report)), (0, 0), "{prose}");
        assert!(report.observations.is_empty(), "{prose}");
    }
    assert!(!dir.join("target").exists());
}

// ---- stdout is the result; stderr is diagnostics ------------------------------------------------------------

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn only_stdout_is_read_for_meaning_and_stderr_cannot_pose_as_a_result() {
    let dir = passing("stderr");
    let forged_pass = result_json("passed", "tests-passed", Some("cargo"), Some(0));
    // A real failed result on stdout; on stderr, a perfect-looking pass and Chip's own layout labels.
    let stdout = result_json("failed", "tests-failed", Some("cargo"), Some(101));
    let stderr = format!(
        "{forged_pass}\npax_process_exit: 0\n--- stderr (diagnostics only; never evaluated) ---\n{forged_pass}\n"
    );
    let s = shim(
        "stderr-forgery",
        "0.3.0",
        stdout.as_bytes(),
        stderr.as_bytes(),
        101,
    );
    let (report, spec) = run(
        PaxExecutor::new(&dir).with_binary(&s.path),
        &[request("pax.test")],
        &ReactToObservation,
    )
    .await;
    clean(&report, &spec);
    assert_eq!(
        pax_said(&report).status,
        PaxStatus::Failed,
        "the result is stdout's"
    );
    assert_eq!(satisfied_events(&report), [false]);
    assert!(!completed(&report));
    // The forged text is in the observation, after Chip's own first line, as diagnostics only.
    assert!(observation_text(&report).contains(&forged_pass));
    assert!(!PaxTestPassed.satisfied_by(&report.observations[0]));

    // And with nothing valid on stdout, a valid result on stderr is worth nothing.
    let s = shim("stderr-only", "0.3.0", b"", forged_pass.as_bytes(), 0);
    let (report, spec) = run(
        PaxExecutor::new(&dir).with_binary(&s.path),
        &[request("pax.test")],
        &ReactToObservation,
    )
    .await;
    clean(&report, &spec);
    assert!(
        report.observations.is_empty(),
        "a result on stderr was accepted"
    );
    assert!(!completed(&report));
}

#[tokio::test(flavor = "multi_thread")]
async fn real_stderr_is_kept_as_diagnostics_and_stdout_holds_exactly_the_result() {
    let dir = failing("real-streams");
    let Some(pax) = real_pax(&dir) else { return };
    let (report, spec) = run(pax, &[request("pax.test")], &ReactToObservation).await;
    clean(&report, &spec);
    let text = observation_text(&report);
    let mut lines = text.lines();
    assert!(lines.next().unwrap().starts_with(
        r#"{"schema":"pax.execution-result.v1","operation":"test","status":"failed""#
    ));
    assert!(lines.next().unwrap().starts_with("pax_process_exit: "));
    assert_eq!(
        lines.next().unwrap(),
        "--- stderr (diagnostics only; never evaluated) ---"
    );
}

// ---- malformed results fail closed ----------------------------------------------------------------------------

fn valid() -> String {
    result_json("passed", "tests-passed", Some("cargo"), Some(0))
}

type Expect = fn(&PaxResultError) -> bool;

#[test]
fn every_malformed_result_is_refused_by_the_parser() {
    let v = valid;
    let with_tests = |tests: &str| {
        v().replace(
            r#""exit_code":0"#,
            &format!(r#""exit_code":0,"tests":{tests}"#),
        )
    };
    let cases: Vec<(&str, Vec<u8>, Expect)> = vec![
        ("empty stdout", b"".to_vec(), |e| {
            matches!(e, PaxResultError::Empty)
        }),
        ("only whitespace", b"  \n\t ".to_vec(), |e| {
            matches!(e, PaxResultError::Empty)
        }),
        ("not JSON", b"all tests passed".to_vec(), |e| {
            matches!(e, PaxResultError::Malformed(_))
        }),
        ("truncated JSON", v().as_bytes()[..40].to_vec(), |e| {
            matches!(e, PaxResultError::Malformed(_))
        }),
        (
            "two documents",
            format!("{}\n{}", v(), v()).into_bytes(),
            |e| matches!(e, PaxResultError::Malformed(_)),
        ),
        (
            "a document and text",
            format!("{}\nTests passed", v()).into_bytes(),
            |e| matches!(e, PaxResultError::Malformed(_)),
        ),
        (
            "text before the document",
            format!("ok\n{}", v()).into_bytes(),
            |e| matches!(e, PaxResultError::Malformed(_)),
        ),
        ("not an object", b"[1,2,3]".to_vec(), |e| {
            matches!(
                e,
                PaxResultError::NotAnObject | PaxResultError::Malformed(_)
            )
        }),
        ("a bare string", b"\"passed\"".to_vec(), |e| {
            matches!(
                e,
                PaxResultError::NotAnObject | PaxResultError::Malformed(_)
            )
        }),
        ("not UTF-8", vec![0xff, 0xfe, b'{', b'}'], |e| {
            matches!(e, PaxResultError::NotUtf8)
        }),
        (
            "missing schema",
            v().replace(r#""schema":"pax.execution-result.v1","#, "")
                .into_bytes(),
            |e| matches!(e, PaxResultError::MissingField("schema")),
        ),
        (
            "wrong schema",
            v().replace("execution-result.v1", "execution-result.v2")
                .into_bytes(),
            |e| matches!(e, PaxResultError::WrongSchema(_)),
        ),
        (
            "an unrelated schema",
            v().replace("pax.execution-result.v1", "other.thing.v1")
                .into_bytes(),
            |e| matches!(e, PaxResultError::WrongSchema(_)),
        ),
        (
            "schema of the wrong type",
            v().replace(r#""schema":"pax.execution-result.v1""#, r#""schema":1"#)
                .into_bytes(),
            |e| matches!(e, PaxResultError::WrongType("schema")),
        ),
        (
            "missing operation",
            v().replace(r#""operation":"test","#, "").into_bytes(),
            |e| matches!(e, PaxResultError::MissingField("operation")),
        ),
        (
            "wrong operation",
            v().replace(r#""operation":"test""#, r#""operation":"build""#)
                .into_bytes(),
            |e| matches!(e, PaxResultError::WrongOperation(_)),
        ),
        (
            "missing status",
            v().replace(r#""status":"passed","#, "").into_bytes(),
            |e| matches!(e, PaxResultError::MissingField("status")),
        ),
        (
            "invalid status",
            v().replace(r#""status":"passed""#, r#""status":"success""#)
                .into_bytes(),
            |e| matches!(e, PaxResultError::InvalidStatus(_)),
        ),
        (
            "status in the wrong case",
            v().replace(r#""status":"passed""#, r#""status":"Passed""#)
                .into_bytes(),
            |e| matches!(e, PaxResultError::InvalidStatus(_)),
        ),
        (
            "status of the wrong type",
            v().replace(r#""status":"passed""#, r#""status":true"#)
                .into_bytes(),
            |e| matches!(e, PaxResultError::WrongType("status")),
        ),
        (
            "missing reason",
            v().replace(r#""reason":"tests-passed","#, "").into_bytes(),
            |e| matches!(e, PaxResultError::MissingField("reason")),
        ),
        (
            "a reason that is prose",
            v().replace("tests-passed", "the tests passed fine")
                .into_bytes(),
            |e| matches!(e, PaxResultError::InvalidReason),
        ),
        (
            "a reason with a newline",
            v().replace("tests-passed", "x\\ny").into_bytes(),
            |e| matches!(e, PaxResultError::InvalidReason),
        ),
        (
            "missing tool",
            v().replace(r#""tool":"cargo","#, "").into_bytes(),
            |e| matches!(e, PaxResultError::MissingField("tool")),
        ),
        (
            "a tool that is a command line",
            v().replace(r#""tool":"cargo""#, r#""tool":"cargo test && rm -rf""#)
                .into_bytes(),
            |e| matches!(e, PaxResultError::InvalidTool),
        ),
        (
            "a tool of the wrong type",
            v().replace(r#""tool":"cargo""#, r#""tool":7"#).into_bytes(),
            |e| matches!(e, PaxResultError::WrongType("tool")),
        ),
        (
            "missing exit_code",
            v().replace(r#","exit_code":0"#, "").into_bytes(),
            |e| matches!(e, PaxResultError::MissingField("exit_code")),
        ),
        (
            "exit_code a string",
            v().replace(r#""exit_code":0"#, r#""exit_code":"0""#)
                .into_bytes(),
            |e| matches!(e, PaxResultError::InvalidExitCode),
        ),
        (
            "exit_code a float",
            v().replace(r#""exit_code":0"#, r#""exit_code":0.5"#)
                .into_bytes(),
            |e| matches!(e, PaxResultError::InvalidExitCode),
        ),
        (
            "exit_code a bool",
            v().replace(r#""exit_code":0"#, r#""exit_code":false"#)
                .into_bytes(),
            |e| matches!(e, PaxResultError::InvalidExitCode),
        ),
        (
            "exit_code out of range",
            v().replace(r#""exit_code":0"#, r#""exit_code":99999999999999999999"#)
                .into_bytes(),
            |e| matches!(e, PaxResultError::InvalidExitCode),
        ),
        (
            "tests a string",
            with_tests(r#""1 passed""#).into_bytes(),
            |e| matches!(e, PaxResultError::InvalidTests(_)),
        ),
        (
            "tests an array",
            with_tests("[1,0,0,0]").into_bytes(),
            |e| matches!(e, PaxResultError::InvalidTests(_)),
        ),
        (
            "tests missing a count",
            with_tests(r#"{"passed":1,"failed":0,"ignored":0}"#).into_bytes(),
            |e| matches!(e, PaxResultError::InvalidTests(_)),
        ),
        (
            "a negative count",
            with_tests(r#"{"passed":-1,"failed":0,"ignored":0,"measured":0}"#).into_bytes(),
            |e| matches!(e, PaxResultError::InvalidTests(_)),
        ),
        (
            "a count that is a string",
            with_tests(r#"{"passed":"1","failed":0,"ignored":0,"measured":0}"#).into_bytes(),
            |e| matches!(e, PaxResultError::InvalidTests(_)),
        ),
        // A repeated field would let the last one silently win; the result is ambiguous and refused.
        (
            "duplicate status",
            v().replace(
                r#""status":"passed","#,
                r#""status":"failed","status":"passed","#,
            )
            .into_bytes(),
            |e| matches!(e, PaxResultError::DuplicateField(k) if k == "status"),
        ),
        (
            "duplicate schema",
            v().replace(r#"{"schema""#, r#"{"schema":"x","schema""#)
                .into_bytes(),
            |e| matches!(e, PaxResultError::DuplicateField(k) if k == "schema"),
        ),
        (
            "duplicate exit_code",
            v().replace(r#""exit_code":0"#, r#""exit_code":101,"exit_code":0"#)
                .into_bytes(),
            |e| matches!(e, PaxResultError::DuplicateField(k) if k == "exit_code"),
        ),
    ];
    for (what, bytes, expected) in &cases {
        match parse_execution_result(bytes) {
            Ok(r) => panic!("{what}: accepted as {r:?}"),
            Err(e) => assert!(expected(&e), "{what}: refused for the wrong reason: {e}"),
        }
    }
    // A control: the untouched document is accepted, so none of the above passed for being broken.
    assert_eq!(
        parse_execution_result(valid().as_bytes()).unwrap().status,
        PaxStatus::Passed
    );
}

#[test]
fn every_status_parses_and_is_kept_as_itself() {
    for (status, expected) in [
        ("passed", PaxStatus::Passed),
        ("failed", PaxStatus::Failed),
        ("error", PaxStatus::Error),
        ("unsupported", PaxStatus::Unsupported),
        ("ambiguous", PaxStatus::Ambiguous),
        ("not_run", PaxStatus::NotRun),
    ] {
        let r = parse_execution_result(
            result_json(status, "some-reason", Some("cargo"), Some(0)).as_bytes(),
        )
        .unwrap();
        assert_eq!(r.status, expected, "{status}");
        assert_eq!(r.status.as_str(), status);
        assert_eq!(
            (r.reason.as_str(), r.tool.as_deref(), r.exit_code, r.tests),
            ("some-reason", Some("cargo"), Some(0), None)
        );
    }
    // Absent optional data is absent, not made up.
    let r = parse_execution_result(
        result_json("ambiguous", "ambiguous-selection", None, None).as_bytes(),
    )
    .unwrap();
    assert_eq!((r.tool, r.exit_code, r.tests), (None, None, None));
    // `tests`, when present, is kept; fields the contract does not define are ignored, never read.
    let with_tests = valid().replace(
        r#""exit_code":0"#,
        r#""exit_code":0,"tests":{"passed":3,"failed":1,"ignored":2,"measured":0},"future_field":{"status":"failed"}"#,
    );
    let r = parse_execution_result(with_tests.as_bytes()).unwrap();
    assert_eq!(
        (
            r.status,
            r.tests.map(|t| (t.passed, t.failed, t.ignored, t.measured))
        ),
        (PaxStatus::Passed, Some((3, 1, 2, 0)))
    );
    // Pretty-printed output (what PAX really writes) is the same document.
    assert!(parse_execution_result(
        b"{\n  \"schema\": \"pax.execution-result.v1\",\n  \"operation\": \"test\",\n  \"status\": \"not_run\",\n  \"reason\": \"no-tests-executed\",\n  \"tool\": \"cargo\",\n  \"exit_code\": 0\n}\n"
    )
    .is_ok());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_result_from_a_running_pax_creates_no_observation_and_no_evidence() {
    // A PAX that ran but wrote something that is not a valid result. The execution happened; its
    // outcome was not established; so nothing is recorded, and nothing is evaluated.
    let dir = passing("malformed-run");
    let bad: Vec<(&str, Vec<u8>)> = vec![
        ("empty stdout", b"".to_vec()),
        ("malformed JSON", b"{\"schema\": ".to_vec()),
        (
            "two documents",
            format!("{}{}", valid(), valid()).into_bytes(),
        ),
        ("wrong schema", valid().replace("v1", "v2").into_bytes()),
        (
            "wrong operation",
            valid()
                .replace(r#""operation":"test""#, r#""operation":"lint""#)
                .into_bytes(),
        ),
        (
            "missing status",
            valid().replace(r#""status":"passed","#, "").into_bytes(),
        ),
        (
            "invalid status",
            valid().replace("passed\"", "success\"").into_bytes(),
        ),
        (
            "invalid exit_code type",
            valid()
                .replace(r#""exit_code":0"#, r#""exit_code":"zero""#)
                .into_bytes(),
        ),
        (
            "invalid tests",
            valid()
                .replace(r#""exit_code":0"#, r#""exit_code":0,"tests":5"#)
                .into_bytes(),
        ),
        ("not UTF-8", vec![0xc3, 0x28]),
        ("larger than a result may be", vec![b' '; 300 * 1024]),
    ];
    for (what, stdout) in bad {
        for exit in [0, 1, 101] {
            let s = shim(
                &format!("malformed-{exit}"),
                "0.3.0",
                &stdout,
                b"native text\n",
                exit,
            );
            let (report, spec) = run(
                PaxExecutor::new(&dir).with_binary(&s.path),
                &[request("pax.test")],
                &ReactToObservation,
            )
            .await;
            clean(&report, &spec);
            assert_eq!(s.calls().len(), 1, "{what}: PAX ran once");
            assert!(
                report.observations.is_empty(),
                "{what} (exit {exit}): an observation was fabricated"
            );
            assert_eq!(evidence(&report), 0, "{what} (exit {exit})");
            assert!(
                !satisfied_events(&report).contains(&true),
                "{what} (exit {exit}): satisfied"
            );
            assert!(!completed(&report), "{what} (exit {exit})");
            assert_eq!(measure_utility(&report, &spec).verified_outputs, 0);
        }
    }
}

// ---- the predicate and the observation layout ------------------------------------------------------------------------

fn observation(kind: ObservationKind, status: ExecutionStatus, output: &str) -> Observation {
    Observation {
        execution_id: ExecutionId::new("e"),
        kind,
        status,
        output: Some(output.to_string()),
        receipt_id: None,
    }
}

fn canonical(status: &str, exit: Option<i64>) -> String {
    let r =
        parse_execution_result(result_json(status, "r", Some("cargo"), exit).as_bytes()).unwrap();
    render_observation(&r, Some(0), b"diagnostics\n")
}

#[test]
fn the_predicate_is_pax_status_passed_and_nothing_else() {
    let done = (
        ObservationKind::ExecutionCompleted,
        ExecutionStatus::Success,
    );
    // Only `passed` satisfies it, whatever the exit code.
    assert!(PaxTestPassed.satisfied_by(&observation(
        done.0,
        done.1,
        &canonical("passed", Some(0))
    )));
    assert!(
        PaxTestPassed.satisfied_by(&observation(done.0, done.1, &canonical("passed", Some(7)))),
        "the exit code is not what decides"
    );
    for status in ["failed", "error", "unsupported", "ambiguous", "not_run"] {
        for exit in [Some(0), Some(1), Some(101), None] {
            assert!(
                !PaxTestPassed.satisfied_by(&observation(done.0, done.1, &canonical(status, exit))),
                "{status} with {exit:?}"
            );
        }
    }
    // A pass that is not a completed, successful observation does not count.
    assert!(!PaxTestPassed.satisfied_by(&observation(
        ObservationKind::ExecutionFailed,
        ExecutionStatus::Failure,
        &canonical("passed", Some(0))
    )));
    // Only line one is read: a pass further down is diagnostics.
    let buried = format!(
        "{}\nignored\n{}",
        canonical("failed", Some(101)).lines().next().unwrap(),
        canonical("passed", Some(0)).lines().next().unwrap()
    );
    assert!(!PaxTestPassed.satisfied_by(&observation(done.0, done.1, &buried)));
    // Anything that is not a valid result on line one is nothing.
    for junk in [
        "",
        "tests passed",
        "test result: ok. 1 passed",
        "status: passed",
        r#"{"status":"passed"}"#,
    ] {
        assert!(
            !PaxTestPassed.satisfied_by(&observation(done.0, done.1, junk)),
            "{junk:?}"
        );
    }
    let none = Observation {
        output: None,
        ..observation(done.0, done.1, "")
    };
    assert!(!PaxTestPassed.satisfied_by(&none));
}

#[test]
fn the_observation_keeps_the_semantic_result_and_the_native_exit_code_apart() {
    let r = parse_execution_result(
        result_json("not_run", "no-tests-executed", Some("cargo"), Some(0)).as_bytes(),
    )
    .unwrap();
    let text = render_observation(&r, Some(0), b"");
    let first = text.lines().next().unwrap();
    assert_eq!(
        first,
        r#"{"schema":"pax.execution-result.v1","operation":"test","status":"not_run","reason":"no-tests-executed","tool":"cargo","exit_code":0}"#
    );
    // Nothing is added to a result that did not have it.
    assert!(!first.contains("\"tests\":"));
    let with = parse_execution_result(
        valid()
            .replace(
                r#""exit_code":0"#,
                r#""exit_code":0,"tests":{"passed":2,"failed":0,"ignored":0,"measured":0}"#,
            )
            .as_bytes(),
    )
    .unwrap();
    assert!(
        render_observation(&with, Some(0), b"")
            .lines()
            .next()
            .unwrap()
            .ends_with(r#""tests":{"passed":2,"failed":0,"ignored":0,"measured":0}}"#)
    );
    // A signal-terminated PAX has no process status, and the layout says exactly that.
    assert!(
        render_observation(&r, None, b"")
            .contains("pax_process_exit: none (terminated by a signal)")
    );
    // The canonical line round-trips through the same strict parser.
    assert_eq!(parse_execution_result(first.as_bytes()).unwrap(), r);
}

// ---- audit: the audit does not trust a result because it appears in a message -----------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_audit_catches_a_completion_no_observation_supports() {
    let dir = passing("audit");
    let Some(pax) = real_pax(&dir) else { return };
    let (report, spec) = run(pax, &[request("pax.test")], &ReactToObservation).await;
    assert!(completed(&report));
    assert!(audit_safety(&report, &spec, &declared()).is_clean());
    // Remove the observation that established the pass: the audit sees a completion nothing supports.
    let mut tampered = report.clone();
    tampered.observations.clear();
    let audit = audit_safety(&tampered, &spec, &declared());
    assert!(audit.false_completions >= 1, "{audit:?}");
    // Replace it with a failed status: also unsupported.
    let mut tampered = report.clone();
    tampered.observations[0].output = Some(canonical("failed", Some(101)));
    assert!(audit_safety(&tampered, &spec, &declared()).false_completions >= 1);
}

#[test]
fn the_predicate_describes_itself_in_terms_of_pax_status() {
    assert!(PaxTestPassed.describe().contains("passed"));
}
