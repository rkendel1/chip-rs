//! `chip-cli work "<goal>" [--json] [--print-reply] [--max-turns N] [--max-executions N]`:
//! bounded autonomous software work in the project in the current directory.
//!
//! The model can inspect and change the project and verify it, only through three declared
//! capabilities: `project.read`, `project.write` and `pax.test`. Chip validates every request,
//! constructs and performs every operation, observes the result, and decides from those
//! observations whether the goal is met. The model's words are never evidence, and its claim that
//! the work is done is not completion.
//!
//! The goal is satisfied only when PAX established the test operation as `passed` *after the last
//! change* Chip observed being written ([`VerifiedChange`]): a pass that predates a later change
//! does not cover it. Completion is the runtime's own decision as soon as that holds
//! ([`CompleteWhenVerified`]); a model that claims completion earlier is refused.
//!
//! Exit status (never a native exit code): 0 verified; 1 not verified (blocked, a limit, or
//! escalated); 2 usage; 3 required infrastructure unavailable (no provider, no usable PAX; nothing
//! ran); 4 runtime failure or a violated safety invariant.

use std::sync::{Arc, Mutex};

use chip_core::{
    Agent, CapabilityAvailability, CapabilityId, CapabilityProvider, CapabilitySet,
    ExecutionObserver, LocalWorkPolicy, ModelDecisionBoundary, Observation, ObservationPredicate,
    SafetyAudit, TestLocalReasoner, WorkDecision, WorkEvent, WorkGoal, WorkId, WorkLimits,
    WorkOutcome, WorkReport, WorkSpec, WorkUtilityMeasurement, WorkView, audit_safety,
    measure_utility, verify_trajectory,
};
use chip_pax::{PAX_TEST_CAPABILITY, PaxExecutor, PaxTestPassed, ResolvedPax};
use chip_project::{
    HOST_PATH_LEAK, NAVIGATION_MISMATCH, OUT_OF_ROOT_WRITE, PATH_ESCAPE, PROJECT_LIST,
    PROJECT_READ, PROJECT_SEARCH, PROJECT_WRITE, ProjectExecutor, host_path_leak_invariant,
    navigation_mismatch_invariant, out_of_root_write_invariant, path_escape_invariant,
    write_summary,
};
use fx_core::ModelProvider;

use crate::horizon::Recording;
use crate::verify::{
    EXIT_NOT_VERIFIED, EXIT_RUNTIME_FAILURE, EXIT_UNAVAILABLE, EXIT_USAGE, EXIT_VERIFIED,
};

pub const DEFAULT_MAX_TURNS: usize = 12;
pub const DEFAULT_MAX_EXECUTIONS: usize = 8;
const LIMIT_CEILING: usize = 50;
const MAX_GOAL_BYTES: usize = 2000;

/// What the model is told about how work is judged: Chip-owned text, not something it can edit.
const RULES: &str = "Inspect and change the project only with project.list, project.search, project.read and project.write (project-relative paths such as src/lib.rs; \".\" is the project root). Chip decides completion: the work is complete only when pax.test passes after your last change that altered a file, as pax.test itself establishes.";

pub fn goal_text(goal: &str) -> String {
    format!("{} {RULES}", goal.trim())
}

/// "PAX established `passed` for the test operation, and a change to a project file was observed
/// before it, with no later change." Read from the recorded observations in order and nothing
/// else.
#[derive(Debug, Clone, Copy, Default)]
pub struct VerifiedChange;

impl ObservationPredicate for VerifiedChange {
    fn describe(&self) -> String {
        "a project file was changed, and after the last change PAX established the test operation as passed".to_string()
    }

    /// Order matters, so no single observation can establish it.
    fn satisfied_by(&self, _observation: &Observation) -> bool {
        false
    }

    fn satisfied_by_trajectory(&self, observations: &[Observation]) -> bool {
        let last_change = observations
            .iter()
            .rposition(|o| write_summary(o).is_some_and(|(_, changed)| changed));
        match last_change {
            Some(at) => observations[at + 1..]
                .iter()
                .any(|o| PaxTestPassed.satisfied_by(o)),
            None => false,
        }
    }
}

/// The runtime completes the work the moment its own evaluation says the goal holds. Until then it
/// has no opinion, and the model is asked.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompleteWhenVerified;

impl LocalWorkPolicy for CompleteWhenVerified {
    fn propose(&self, view: &WorkView<'_>) -> Option<WorkDecision> {
        VerifiedChange
            .satisfied_by_trajectory(view.observations)
            .then(|| WorkDecision::Complete {
                summary: "pax.test passed after the last change".to_string(),
            })
    }
}

/// The one supported piece of work, over one project.
pub fn spec(goal: &str, root: &std::path::Path, limits: WorkLimits) -> WorkSpec {
    WorkSpec::new(WorkId::new("work"), WorkGoal::new(goal_text(goal)))
        .with_limits(limits)
        .with_required_observation(Arc::new(VerifiedChange))
        .with_observation_invariant(path_escape_invariant(root))
        .with_observation_invariant(out_of_root_write_invariant(root))
        .with_observation_invariant(host_path_leak_invariant(root))
        .with_observation_invariant(navigation_mismatch_invariant(root))
        // The project changes between requests, so none of these may ever be answered from memory.
        .with_evidence_reuse_prohibited(CapabilityId::new(PROJECT_LIST).unwrap())
        .with_evidence_reuse_prohibited(CapabilityId::new(PROJECT_SEARCH).unwrap())
        .with_evidence_reuse_prohibited(CapabilityId::new(PROJECT_READ).unwrap())
        .with_evidence_reuse_prohibited(CapabilityId::new(PROJECT_WRITE).unwrap())
        .with_evidence_reuse_prohibited(CapabilityId::new(PAX_TEST_CAPABILITY).unwrap())
}

fn declared() -> Vec<CapabilityId> {
    [
        PROJECT_LIST,
        PROJECT_SEARCH,
        PROJECT_READ,
        PROJECT_WRITE,
        PAX_TEST_CAPABILITY,
    ]
    .iter()
    .map(|c| CapabilityId::new(*c).unwrap())
    .collect()
}

/// How many tokens one model reply may spend. A reply that writes a whole file needs room for it;
/// the model boundary's default is far smaller. A budget, not an authority.
pub const WORK_MAX_OUTPUT_TOKENS: u32 = 2048;

/// Which model did the work. The endpoint is its identity only (scheme, host, port), never a
/// credential or a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub provider: String,
    pub model: String,
    pub endpoint: String,
}

/// What a run of software work established.
pub struct SoftwareWork {
    pub goal: String,
    pub report: WorkReport,
    pub audit: SafetyAudit,
    pub trajectory_violations: usize,
    pub utility: WorkUtilityMeasurement,
    /// The loop's last evaluation of the goal; `None` if it never evaluated one.
    pub goal_satisfied: Option<bool>,
    /// The goal, re-evaluated from the recorded observations by the predicate itself.
    pub verified: bool,
    /// Successful writes, and those that changed a file's content.
    pub writes: usize,
    pub changed_writes: usize,
    pub paths_written: Vec<String>,
    /// Set by the caller that chose the model; `None` when it was not a configured one.
    pub identity: Option<Identity>,
}

impl SoftwareWork {
    /// How many capabilities the model asked for (valid requests; a refused one never became one).
    pub fn capability_requests(&self) -> usize {
        self.report
            .events
            .iter()
            .filter(|e| matches!(e, WorkEvent::CapabilityRequested { .. }))
            .count()
    }

    /// The work ended on a request whose inputs did not match the capability's declaration (an
    /// `inputs` field on a capability that takes none, a missing or undeclared input).
    pub fn invalid_inputs(&self) -> usize {
        usize::from(matches!(
            &self.report.outcome,
            WorkOutcome::Blocked { reason } if reason.starts_with("invalid capability input")
        ))
    }

    pub fn lists(&self) -> usize {
        self.count(PROJECT_LIST)
    }
    pub fn searches(&self) -> usize {
        self.count(PROJECT_SEARCH)
    }
    pub fn reads(&self) -> usize {
        self.count(PROJECT_READ)
    }
    pub fn pax_executions(&self) -> usize {
        self.count(PAX_TEST_CAPABILITY)
    }
    fn count(&self, capability: &str) -> usize {
        self.utility
            .executions_by_capability
            .get(capability)
            .copied()
            .unwrap_or(0)
    }

    /// Writes that changed a file, counted only when the goal was verified: unverified change is
    /// not useful work, however plausible it looked.
    pub fn useful_writes(&self) -> usize {
        if self.verified {
            self.changed_writes
        } else {
            0
        }
    }

    pub fn invariants_hold(&self) -> bool {
        self.audit.is_clean() && self.trajectory_violations == 0
    }

    pub fn exit_status(&self) -> i32 {
        if !self.invariants_hold() {
            return EXIT_RUNTIME_FAILURE;
        }
        match &self.report.outcome {
            WorkOutcome::Completed { .. } if self.verified => EXIT_VERIFIED,
            WorkOutcome::Completed { .. } | WorkOutcome::Failed { .. } => EXIT_RUNTIME_FAILURE,
            WorkOutcome::Blocked { .. }
            | WorkOutcome::LimitReached { .. }
            | WorkOutcome::Escalated { .. } => EXIT_NOT_VERIFIED,
        }
    }
}

/// Runs the work through the existing loop with the project, PAX and the given model.
pub async fn run_software_work(
    model: Arc<dyn ModelProvider>,
    model_name: String,
    root: &std::path::Path,
    pax: PaxExecutor,
    goal: &str,
    limits: WorkLimits,
    policy: &dyn LocalWorkPolicy,
) -> SoftwareWork {
    let set = Arc::new(
        CapabilitySet::new()
            .with(Arc::new(ProjectExecutor::new(root)))
            .with(Arc::new(pax)),
    );
    let agent = Agent::with_model(model, model_name)
        .with_capabilities(set.clone())
        .with_executor(set)
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(Arc::new(TestLocalReasoner::default()))
        .with_max_output_tokens(WORK_MAX_OUTPUT_TOKENS);
    let spec = spec(goal, root, limits);
    let report = agent.run_work(&spec, policy, &ModelDecisionBoundary).await;
    let audit = audit_safety(&report, &spec, &declared());
    let trajectory_violations = verify_trajectory(&report.events, &spec.limits).len();
    let utility = measure_utility(&report, &spec);
    let goal_satisfied = report.events.iter().rev().find_map(|e| match e {
        WorkEvent::GoalEvaluated { satisfied, .. } => Some(*satisfied),
        _ => None,
    });
    let verified = VerifiedChange.satisfied_by_trajectory(&report.observations);
    let written: Vec<(String, bool)> = report
        .observations
        .iter()
        .filter_map(write_summary)
        .collect();
    SoftwareWork {
        goal: goal.trim().to_string(),
        audit,
        trajectory_violations,
        utility,
        goal_satisfied,
        verified,
        writes: written.len(),
        changed_writes: written.iter().filter(|(_, changed)| *changed).count(),
        paths_written: written.into_iter().map(|(p, _)| p).collect(),
        identity: None,
        report,
    }
}

fn heading(outcome: &WorkOutcome) -> &'static str {
    match outcome {
        WorkOutcome::Completed { .. } => "Work completed.",
        WorkOutcome::Blocked { .. } => "Work blocked.",
        WorkOutcome::Failed { .. } => "Work failed.",
        WorkOutcome::LimitReached { .. } => "Work stopped at a limit.",
        WorkOutcome::Escalated { .. } => "Work escalated.",
    }
}

fn outcome_reason(outcome: &WorkOutcome) -> Option<String> {
    match outcome {
        WorkOutcome::Completed { .. } => None,
        WorkOutcome::Blocked { reason }
        | WorkOutcome::Failed { reason }
        | WorkOutcome::Escalated { reason } => Some(reason.clone()),
        WorkOutcome::LimitReached { limit } => Some(format!("{} limit reached", limit.name())),
    }
}

/// What PAX last said, from the last observation that is a PAX result.
fn last_pax(work: &SoftwareWork) -> Option<chip_pax::PaxExecutionResult> {
    work.report.observations.iter().rev().find_map(|o| {
        let first = o.output.as_deref()?.lines().next()?;
        chip_pax::parse_execution_result(first.as_bytes()).ok()
    })
}

fn by_capability(work: &SoftwareWork) -> String {
    let parts: Vec<String> = work
        .utility
        .executions_by_capability
        .iter()
        .map(|(c, n)| format!("{c} x{n}"))
        .collect();
    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join(", ")
    }
}

pub fn render_human(w: &SoftwareWork, pax: &ResolvedPax) -> String {
    let m = w.report.measurement();
    let u = &w.utility;
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push('\n');
    };
    line(heading(&w.report.outcome).to_string());
    line(format!("Goal: {}", w.goal));
    if let Some(id) = &w.identity {
        line(format!("Provider: {}", id.provider));
        line(format!("Model: {}", id.model));
        line(format!("Endpoint: {}", id.endpoint));
    }
    line(format!("Executed: {}", by_capability(w)));
    line(format!(
        "Lists: {}  Searches: {}  Reads: {}  Writes: {} ({} changed)  Tests: {}",
        w.lists(),
        w.searches(),
        w.reads(),
        w.writes,
        w.changed_writes,
        w.pax_executions()
    ));
    if w.writes > 0 {
        line(format!("Wrote: {}", w.paths_written.join(", ")));
    }
    match last_pax(w) {
        Some(r) => {
            let tests = r.tests.map_or(String::new(), |t| {
                format!(
                    " ({} passed, {} failed, {} ignored)",
                    t.passed, t.failed, t.ignored
                )
            });
            line(format!("Last test result: {}{tests}", r.status.as_str()));
        }
        None => line("Last test result: none (pax.test was not run)".to_string()),
    }
    line(format!(
        "Verified: {}",
        if w.verified {
            "yes (pax.test passed after the last change)"
        } else {
            "no"
        }
    ));
    if let Some(reason) = outcome_reason(&w.report.outcome) {
        line(format!("Outcome: {reason}"));
    }
    line(format!(
        "Work: {} turn(s), {} model call(s), {} execution(s), {} failed observation(s), {} recovery attempt(s)",
        m.turns, m.model_calls, u.executions, u.failed_observations, u.recoveries
    ));
    line(format!(
        "Latency: total {:.0} ms (model {:.0} ms, execution {:.0} ms)",
        m.total_latency.as_secs_f64() * 1000.0,
        m.model_latency.as_secs_f64() * 1000.0,
        m.compute_latency.as_secs_f64() * 1000.0
    ));
    line(format!(
        "Tokens: {}",
        u.total_tokens
            .map_or("not reported".to_string(), |t| t.to_string())
    ));
    line(
        "Evidence: observations of real operations; no cryptographic execution receipt".to_string(),
    );
    line(format!("PAX: {} ({})", pax.version, pax.path.display()));
    if w.invariants_hold() {
        line("Audit: clean".to_string());
    } else {
        line(format!(
            "Audit: VIOLATION {:?} (trajectory violations: {})",
            w.audit, w.trajectory_violations
        ));
    }
    let a = &w.audit;
    line(format!(
        "  unauthorized executions: {}",
        a.unauthorized_executions
    ));
    line(format!("  path escapes: {}", a.violations_of(PATH_ESCAPE)));
    line(format!(
        "  out-of-root writes: {}",
        a.violations_of(OUT_OF_ROOT_WRITE)
    ));
    line(format!("  forged observations: {}", forged_observations(a)));
    line(format!("  false completions: {}", a.false_completions));
    out
}

/// Observations that no real execution, filesystem state or valid chain accounts for.
fn forged_observations(a: &SafetyAudit) -> usize {
    a.observation_without_execution
        + a.evidence_without_observation
        + a.violations_of(NAVIGATION_MISMATCH)
}

pub fn render_json(w: &SoftwareWork, pax: &ResolvedPax) -> String {
    let m = w.report.measurement();
    let u = &w.utility;
    let last = last_pax(w);
    let a = &w.audit;
    let measurement: serde_json::Value =
        serde_json::from_str(&crate::work_demo::measurement_json("work", &m))
            .expect("the measurement is JSON");
    let id = w.identity.as_ref();
    serde_json::json!({
        "command": "work",
        "provider": id.map(|i| i.provider.as_str()),
        "model": id.map(|i| i.model.as_str()),
        "endpoint": id.map(|i| i.endpoint.as_str()),
        "goal": w.goal,
        "terminal_state": m.terminal_state().name(),
        "outcome_reason": outcome_reason(&w.report.outcome),
        "goal_satisfied": w.goal_satisfied,
        "verified": w.verified,
        "executions_by_capability": u.executions_by_capability,
        "lists": w.lists(),
        "searches": w.searches(),
        "reads": w.reads(),
        "writes": w.writes,
        "changed_writes": w.changed_writes,
        "useful_writes": w.useful_writes(),
        "tests": w.pax_executions(),
        "pax_executions": w.pax_executions(),
        "paths_written": w.paths_written,
        "failed_observations": u.failed_observations,
        "recoveries": u.recoveries,
        "capability_requests": w.capability_requests(),
        "invalid_decisions": u.invalid_decisions,
        "invalid_inputs": w.invalid_inputs(),
        "useful_work_per_model_call": u.work_per_model_call(),
        "useful_work_per_execution": u.work_per_execution(),
        "pax": {
            "version": pax.version,
            "last_status": last.as_ref().map(|r| r.status.as_str()),
            "last_reason": last.as_ref().map(|r| r.reason.as_str()),
            "last_exit_code": last.as_ref().and_then(|r| r.exit_code),
        },
        "receipt": serde_json::Value::Null,
        "audit": {
            "clean": w.invariants_hold(),
            "unauthorized_executions": a.unauthorized_executions,
            "unauthorized_completions": a.unauthorized_completions,
            "false_completions": a.false_completions,
            "evidence_without_observation": a.evidence_without_observation,
            "observation_without_execution": a.observation_without_execution,
            "execution_without_valid_request": a.execution_without_valid_request,
            "limit_violations": a.limit_violations,
            "stale_evidence_reuse": a.stale_evidence_reuse,
            "events_after_terminal": a.events_after_terminal,
            "path_escape": a.violations_of(PATH_ESCAPE),
            "out_of_root_write": a.violations_of(OUT_OF_ROOT_WRITE),
            "host_path_leak": a.violations_of(HOST_PATH_LEAK),
            "navigation_mismatch": a.violations_of(NAVIGATION_MISMATCH),
            "forged_observations": forged_observations(a),
        },
        "exit_status": w.exit_status(),
        "measurement": measurement,
    })
    .to_string()
}

fn usage() -> i32 {
    eprintln!(
        "usage: chip-cli work \"<goal>\" [--provider P] [--model M] [--endpoint URL] [--json] [--print-reply] [--max-turns N] [--max-executions N]"
    );
    eprintln!(
        "       provider/model/endpoint: command line, then CHIP_PROVIDER / CHIP_MODEL / CHIP_ENDPOINT, then the provider's default endpoint; a model is always required"
    );
    eprintln!("       works on the project in the current directory");
    EXIT_USAGE
}

fn limit(value: Option<&String>, name: &str) -> Result<usize, i32> {
    match value.and_then(|v| v.parse::<usize>().ok()) {
        Some(n) if (1..=LIMIT_CEILING).contains(&n) => Ok(n),
        _ => {
            eprintln!("error: {name} needs a number from 1 to {LIMIT_CEILING}");
            Err(usage())
        }
    }
}

fn value(args: &[String], at: usize, flag: &str) -> Result<String, i32> {
    match args.get(at).map(|v| v.trim()) {
        Some(v) if !v.is_empty() && !v.starts_with("--") => Ok(v.to_string()),
        _ => {
            eprintln!("error: {flag} needs a value");
            Err(usage())
        }
    }
}

pub async fn work(args: &[String]) -> i32 {
    let mut selection = crate::provider_selection::Selection::default();
    let (mut json, mut print_reply) = (false, false);
    let mut goal: Option<String> = None;
    let (mut max_turns, mut max_executions) = (DEFAULT_MAX_TURNS, DEFAULT_MAX_EXECUTIONS);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json = true,
            "--print-reply" => print_reply = true,
            "--provider" | "--model" | "--endpoint" => {
                let flag = args[i].clone();
                i += 1;
                let given = match value(args, i, &flag) {
                    Ok(v) => v,
                    Err(code) => return code,
                };
                match flag.as_str() {
                    "--provider" => selection.provider = Some(given),
                    "--model" => selection.model = Some(given),
                    _ => selection.endpoint = Some(given),
                }
            }
            "--max-turns" => {
                i += 1;
                match limit(args.get(i), "--max-turns") {
                    Ok(n) => max_turns = n,
                    Err(code) => return code,
                }
            }
            "--max-executions" => {
                i += 1;
                match limit(args.get(i), "--max-executions") {
                    Ok(n) => max_executions = n,
                    Err(code) => return code,
                }
            }
            flag if flag.starts_with("--") => {
                eprintln!("error: unexpected argument `{flag}`");
                return usage();
            }
            text if goal.is_none() => goal = Some(text.to_string()),
            other => {
                eprintln!("error: unexpected argument `{other}`; quote the goal as one argument");
                return usage();
            }
        }
        i += 1;
    }
    let Some(goal) = goal.filter(|g| !g.trim().is_empty()) else {
        eprintln!("error: a goal is required");
        return usage();
    };
    if goal.len() > MAX_GOAL_BYTES || goal.chars().any(|c| c.is_control() && c != '\n') {
        eprintln!("error: the goal must be plain text of at most {MAX_GOAL_BYTES} bytes");
        return usage();
    }
    let root = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("error: no current directory ({e})");
            return EXIT_UNAVAILABLE;
        }
    };
    let config =
        match crate::provider_selection::resolve(&selection, |name| std::env::var(name).ok()) {
            Ok(config) => config,
            Err(e) => {
                eprintln!("error: no model is selected ({e}); nothing was run");
                return EXIT_UNAVAILABLE;
            }
        };
    let model_name = config.model.to_string();
    let identity = Identity {
        provider: config.provider.clone(),
        model: model_name.clone(),
        endpoint: crate::provider_selection::endpoint_identity(&config.endpoint),
    };
    let provider = match fx_provider_http::HttpProvider::new(config) {
        Ok(provider) => provider,
        Err(e) => {
            eprintln!("error: the model provider is unusable ({e}); nothing was run");
            return EXIT_UNAVAILABLE;
        }
    };
    // Verification is how the goal is established, so without a usable PAX nothing is attempted.
    let pax = PaxExecutor::new(&root);
    let resolved = match pax.resolve().await {
        Ok(resolved) => resolved,
        Err(why) => {
            eprintln!("error: PAX unavailable ({why}); nothing was run");
            return EXIT_UNAVAILABLE;
        }
    };
    let project = ProjectExecutor::new(&root);
    for id in [PROJECT_LIST, PROJECT_SEARCH, PROJECT_READ, PROJECT_WRITE] {
        let ready = project.availability(&CapabilityId::new(id).unwrap()).await;
        if !matches!(ready, CapabilityAvailability::Available) {
            eprintln!("error: {id} is unavailable; nothing was run");
            return EXIT_UNAVAILABLE;
        }
    }

    let replies = Arc::new(Mutex::new(Vec::new()));
    let model = Arc::new(Recording {
        inner: provider,
        replies: replies.clone(),
    });
    let limits = WorkLimits {
        max_turns,
        max_executions,
    };
    let mut result = run_software_work(
        model,
        model_name,
        &root,
        pax,
        &goal,
        limits,
        &CompleteWhenVerified,
    )
    .await;
    result.identity = Some(identity.clone());
    // The selected model is the only one ever asked. If it cannot answer, the run says so and stops:
    // no other provider or model is tried.
    let calls: Vec<bool> = result
        .report
        .events
        .iter()
        .filter_map(|e| match e {
            WorkEvent::ModelCalled { succeeded, .. } => Some(*succeeded),
            _ => None,
        })
        .collect();
    if calls.contains(&false) {
        let why = outcome_reason(&result.report.outcome).unwrap_or_default();
        eprintln!(
            "error: the selected model did not answer ({} {} at {}): {why}. No other provider or model was tried.",
            identity.provider, identity.model, identity.endpoint
        );
        if calls.first() == Some(&false) && result.utility.executions == 0 {
            return EXIT_UNAVAILABLE;
        }
    }
    if json {
        println!("{}", render_json(&result, &resolved));
    } else {
        print!("{}", render_human(&result, &resolved));
    }
    if print_reply {
        for reply in replies.lock().unwrap().iter() {
            eprintln!("Model reply:\n{reply}");
        }
    }
    if !result.invariants_hold() {
        eprintln!(
            "SAFETY INVARIANT VIOLATED: {:?} (trajectory violations: {})",
            result.audit, result.trajectory_violations
        );
    }
    result.exit_status()
}

#[cfg(test)]
mod tests {
    //! Real filesystem, real PAX 0.3.0, real Cargo. Only the *model* is scripted: a fixture that
    //! replies with fixed text through the real strict decision boundary. The runtime's loop,
    //! capabilities, writes, test runs and observations are the production ones; nothing about the
    //! scripted sequence is known to the runtime. Skipped (and said so) if PAX is not installed.

    use std::collections::{BTreeMap, VecDeque};
    use std::path::{Path, PathBuf};

    use chip_core::{ExecutionEvent, ObservationKind};
    use fx_core::{FxError, ModelRequest, ModelResponse, Usage};

    use super::*;

    struct Script {
        replies: Mutex<VecDeque<String>>,
        seen: Mutex<Vec<String>>,
    }

    impl Script {
        fn new(replies: &[String]) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.iter().cloned().collect()),
                seen: Mutex::new(Vec::new()),
            })
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
                .ok_or_else(|| FxError::Provider("unscripted model call".into()))?;
            Ok(ModelResponse::new("m", reply, Usage::new(3, 2)))
        }
    }

    /// Always asks the model (to test what the model may claim).
    struct AskModel;

    impl LocalWorkPolicy for AskModel {
        fn propose(&self, _v: &WorkView<'_>) -> Option<WorkDecision> {
            None
        }
    }

    const GOAL: &str = "Add a function `canonical_fingerprint` that returns the canonical form of a payload, so the project's tests pass.";
    const OLD_LIB: &str = "pub fn payload_len(payload: &str) -> usize {\n    payload.len()\n}\n";
    const RIGHT: &str = "pub fn payload_len(payload: &str) -> usize {\n    payload.len()\n}\n\n/// Pairs of `k=v` in sorted order, joined by `&`.\npub fn canonical_fingerprint(payload: &str) -> String {\n    let mut pairs: Vec<&str> = payload.split('&').collect();\n    pairs.sort();\n    pairs.join(\"&\")\n}\n";
    const WRONG: &str = "pub fn payload_len(payload: &str) -> usize {\n    payload.len()\n}\n\npub fn canonical_fingerprint(payload: &str) -> String {\n    payload.to_string()\n}\n";
    const TESTS: &str = "use fpfixture::canonical_fingerprint;\n\n#[test]\nfn pairs_are_sorted_and_joined() {\n    assert_eq!(canonical_fingerprint(\"b=2&a=1\"), \"a=1&b=2\");\n}\n\n#[test]\nfn an_already_canonical_payload_is_unchanged() {\n    assert_eq!(canonical_fingerprint(\"a=1&b=2\"), \"a=1&b=2\");\n}\n\n#[test]\nfn a_single_pair_is_unchanged() {\n    assert_eq!(canonical_fingerprint(\"z=9\"), \"z=9\");\n}\n";

    struct Fx {
        base: PathBuf,
        root: PathBuf,
        outside: PathBuf,
    }

    /// A real Rust project whose tests fail until `canonical_fingerprint` exists.
    fn fixture(tag: &str) -> Fx {
        let base = std::env::temp_dir().join(format!("chip-work-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (root, outside) = (base.join("project"), base.join("outside"));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            format!("[package]\nname = \"fpfixture_{}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nname = \"fpfixture\"\n", tag.replace('-', "_")),
        )
        .unwrap();
        std::fs::write(root.join("src/lib.rs"), OLD_LIB).unwrap();
        std::fs::write(root.join("tests/fingerprint.rs"), TESTS).unwrap();
        std::fs::write(outside.join("secret.txt"), "outside secret\n").unwrap();
        Fx {
            base,
            root,
            outside,
        }
    }

    async fn pax_for(root: &Path) -> Option<PaxExecutor> {
        let pax = PaxExecutor::new(root);
        match pax.resolve().await {
            Ok(_) => Some(pax),
            Err(why) => {
                eprintln!("SKIPPED: {why}");
                None
            }
        }
    }

    fn read(path: &str) -> String {
        format!(
            r#"{{"decision":"request_capability","capability":"project.read","inputs":{{"path":{}}}}}"#,
            serde_json::to_string(path).unwrap()
        )
    }

    fn write(path: &str, content: &str) -> String {
        format!(
            r#"{{"decision":"request_capability","capability":"project.write","inputs":{{"path":{},"content":{}}}}}"#,
            serde_json::to_string(path).unwrap(),
            serde_json::to_string(content).unwrap()
        )
    }

    fn test_run() -> String {
        r#"{"decision":"request_capability","capability":"pax.test"}"#.to_string()
    }

    fn claim() -> String {
        r#"{"decision":"complete","summary":"All done, the tests pass."}"#.to_string()
    }

    const LIMITS: WorkLimits = WorkLimits {
        max_turns: 12,
        max_executions: 8,
    };

    async fn go(
        fx: &Fx,
        pax: PaxExecutor,
        script: &Arc<Script>,
        limits: WorkLimits,
        policy: &dyn LocalWorkPolicy,
    ) -> SoftwareWork {
        run_software_work(
            script.clone(),
            "scripted".into(),
            &fx.root,
            pax,
            GOAL,
            limits,
            policy,
        )
        .await
    }

    fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                if rel == "target" || rel.starts_with("target/") || rel == "Cargo.lock" {
                    continue;
                }
                if path.is_dir() {
                    walk(base, &path, out);
                } else if let Ok(bytes) = std::fs::read(&path) {
                    out.insert(rel, bytes);
                }
            }
        }
        walk(dir, dir, &mut out);
        out
    }

    fn goal_trail(w: &SoftwareWork) -> Vec<bool> {
        w.report
            .events
            .iter()
            .filter_map(|e| match e {
                WorkEvent::GoalEvaluated { satisfied, .. } => Some(*satisfied),
                _ => None,
            })
            .collect()
    }

    fn count(w: &SoftwareWork, pick: fn(&WorkEvent) -> bool) -> usize {
        w.report.events.iter().filter(|e| pick(e)).count()
    }

    fn started(w: &SoftwareWork) -> usize {
        count(w, |e| {
            matches!(
                e,
                WorkEvent::Execution(ExecutionEvent::ExecutionStarted { .. })
            )
        })
    }

    fn completed(w: &SoftwareWork) -> bool {
        matches!(w.report.outcome, WorkOutcome::Completed { .. })
    }

    fn tidy(w: &SoftwareWork) {
        w.audit.assert_clean();
        assert_eq!(w.trajectory_violations, 0);
        assert_eq!(w.audit.violations_of(PATH_ESCAPE), 0);
        assert_eq!(w.audit.violations_of(OUT_OF_ROOT_WRITE), 0);
        assert_eq!(w.audit.violations_of(HOST_PATH_LEAK), 0);
        assert_eq!(w.audit.violations_of(NAVIGATION_MISMATCH), 0);
        assert_eq!(w.audit.stale_evidence_reuse, 0);
        assert_eq!(w.audit.events_after_terminal, 0);
    }

    fn resolved_pax() -> ResolvedPax {
        ResolvedPax {
            path: PathBuf::from("/usr/local/bin/pax"),
            version: "0.3.0".into(),
        }
    }

    // ---- the useful task ----------------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn real_work_inspect_change_verify_complete() {
        let fx = fixture("e2e");
        let Some(pax) = pax_for(&fx.root).await else {
            return;
        };
        // The fixture really fails before any work: PAX says so, about the real project.
        let before = {
            let probe = Script::new(&[test_run()]);
            let w = go(&fx, pax.clone(), &probe, LIMITS, &AskModel).await;
            last_pax(&w).expect("a PAX result").status
        };
        assert_eq!(
            before,
            chip_pax::PaxStatus::Failed,
            "the fixture must fail before the work"
        );
        assert!(!fx.root.join("target").join("never").exists());

        let script = Script::new(&[
            read("tests/fingerprint.rs"),
            read("src/lib.rs"),
            write("src/lib.rs", RIGHT),
            test_run(),
        ]);
        let w = go(&fx, pax, &script, LIMITS, &CompleteWhenVerified).await;
        tidy(&w);
        assert!(completed(&w), "{:?}", w.report.outcome);
        assert!(w.verified);
        assert_eq!(w.exit_status(), EXIT_VERIFIED);
        // Real filesystem state, written by Chip, not by the model.
        assert_eq!(
            std::fs::read_to_string(fx.root.join("src/lib.rs")).unwrap(),
            RIGHT
        );
        assert_eq!(
            std::fs::read_to_string(fx.root.join("tests/fingerprint.rs")).unwrap(),
            TESTS
        );
        // The counts the report needs.
        assert_eq!(
            (
                w.reads(),
                w.writes,
                w.changed_writes,
                w.useful_writes(),
                w.pax_executions()
            ),
            (2, 1, 1, 1, 1)
        );
        assert_eq!(w.utility.executions, 4);
        assert_eq!(
            w.report.measurement().model_calls,
            4,
            "the runtime completed it: no fifth call"
        );
        assert_eq!(w.utility.verified_outputs, 1);
        assert_eq!(last_pax(&w).unwrap().status, chip_pax::PaxStatus::Passed);
        // Reality, step by step: each observation was evaluated; only the last satisfied the goal.
        assert_eq!(goal_trail(&w), [false, false, false, true]);
        assert_eq!(
            count(&w, |e| matches!(e, WorkEvent::EvidenceRecorded { .. })),
            4
        );
        assert_eq!(
            count(&w, |e| matches!(e, WorkEvent::EvidenceReused { .. })),
            0
        );
        assert!(w.report.observations.iter().all(|o| o.receipt_id.is_none()));
        // The model was shown the file's real content as an observation, and no host path.
        let seen = script.seen.lock().unwrap();
        assert!(
            seen[1].contains("canonical_fingerprint"),
            "the test file was not in the next context"
        );
        assert!(
            seen.iter().all(|m| !m.contains(fx.root.to_str().unwrap())),
            "a host path reached the model"
        );
        // The JSON report states all of it.
        let json: serde_json::Value =
            serde_json::from_str(&render_json(&w, &resolved_pax())).unwrap();
        assert_eq!(json["terminal_state"], "completed");
        assert_eq!(
            (
                json["verified"].as_bool(),
                json["reads"].as_u64(),
                json["writes"].as_u64(),
                json["pax_executions"].as_u64()
            ),
            (Some(true), Some(2), Some(1), Some(1))
        );
        assert_eq!(json["audit"]["path_escape"], 0);
        assert_eq!(json["audit"]["out_of_root_write"], 0);
        assert_eq!(json["executions_by_capability"]["project.write"], 1);
        assert_eq!(json["paths_written"][0], "src/lib.rs");
        assert_eq!(json["receipt"], serde_json::Value::Null);
        assert!(render_human(&w, &resolved_pax()).starts_with("Work completed.\n"));
        let _ = fx.base;
    }

    // ---- wrong judgment is recoverable, and stale reality is never reused ---------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn wrong_judgments_become_real_failures_and_recovery_runs_the_tests_again() {
        let fx = fixture("recover");
        let Some(pax) = pax_for(&fx.root).await else {
            return;
        };
        let script = Script::new(&[
            read("src/main.rs"), // wrong file: reality says it is not there
            read("tests/fingerprint.rs"),
            write("src/lib.rs", WRONG), // a plausible but wrong change
            test_run(),                 // reality: it fails
            write("src/lib.rs", RIGHT), // recovery, through another bounded judgment
            test_run(),                 // the identical request: it must run again
        ]);
        let w = go(&fx, pax, &script, LIMITS, &CompleteWhenVerified).await;
        tidy(&w);
        assert!(completed(&w), "{:?}", w.report.outcome);
        assert_eq!(
            w.pax_executions(),
            2,
            "the second pax.test must not be answered from the first"
        );
        assert_eq!(
            count(&w, |e| matches!(e, WorkEvent::EvidenceReused { .. })),
            0
        );
        assert_eq!(
            w.utility.failed_observations, 2,
            "the missing file and the failing tests"
        );
        assert!(w.utility.recoveries >= 2, "{}", w.utility.recoveries);
        assert_eq!(w.writes, 2);
        assert_eq!(w.changed_writes, 2);
        assert_eq!(goal_trail(&w), [false, false, false, false, false, true]);
        assert_eq!(
            std::fs::read_to_string(fx.root.join("src/lib.rs")).unwrap(),
            RIGHT
        );
        // The model's second context carried the failure as an observation, not a repaired fact.
        assert!(script.seen.lock().unwrap()[1].contains("not_found"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_pass_does_not_cover_a_later_change() {
        let fx = fixture("supersede");
        let Some(pax) = pax_for(&fx.root).await else {
            return;
        };
        let touched = format!("{RIGHT}\n// touched after the verification\n");
        // The model keeps going after the pass (the runtime policy stands aside so the claim is the model's).
        let script = Script::new(&[
            write("src/lib.rs", RIGHT),
            test_run(),
            write("src/lib.rs", &touched),
            claim(),
        ]);
        let w = go(&fx, pax.clone(), &script, LIMITS, &AskModel).await;
        tidy(&w);
        assert!(
            matches!(&w.report.outcome, WorkOutcome::Blocked { reason } if reason.starts_with("completion refused")),
            "{:?}",
            w.report.outcome
        );
        assert_eq!(
            goal_trail(&w),
            [false, true, false],
            "satisfied by the pass, unmet again by the later change"
        );
        assert!(!w.verified);
        assert_eq!(
            w.useful_writes(),
            0,
            "unverified change is not counted as useful work"
        );
        assert_eq!(w.exit_status(), EXIT_NOT_VERIFIED);
        // Verified again after the last change, the same claim stands.
        let fx = fixture("supersede2");
        let script = Script::new(&[
            write("src/lib.rs", RIGHT),
            test_run(),
            write("src/lib.rs", &touched),
            test_run(),
            claim(),
        ]);
        let w = go(
            &fx,
            pax_for(&fx.root).await.unwrap(),
            &script,
            LIMITS,
            &AskModel,
        )
        .await;
        tidy(&w);
        assert!(completed(&w), "{:?}", w.report.outcome);
        assert_eq!(goal_trail(&w), [false, true, false, true]);
        assert_eq!(w.pax_executions(), 2);
    }

    // ---- completion belongs to the runtime ------------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn a_claim_of_completion_is_refused_unless_reality_supports_it() {
        for (what, replies) in [
            ("before anything", vec![claim()]),
            ("after only reading", vec![read("src/lib.rs"), claim()]),
            (
                "after a write, unverified",
                vec![write("src/lib.rs", RIGHT), claim()],
            ),
            (
                "after a failed verification",
                vec![write("src/lib.rs", WRONG), test_run(), claim()],
            ),
            (
                "after a failed verification with no change",
                vec![test_run(), claim()],
            ),
        ] {
            let fx = fixture("claims");
            let Some(pax) = pax_for(&fx.root).await else {
                return;
            };
            let script = Script::new(&replies);
            let w = go(&fx, pax, &script, LIMITS, &CompleteWhenVerified).await;
            tidy(&w);
            assert!(!completed(&w), "{what}: a claim completed the work");
            assert!(
                matches!(&w.report.outcome, WorkOutcome::Blocked { reason } if reason.starts_with("completion refused")),
                "{what}: {:?}",
                w.report.outcome
            );
            assert!(!w.verified, "{what}");
            assert_eq!(w.exit_status(), EXIT_NOT_VERIFIED, "{what}");
            assert!(
                !w.report
                    .events
                    .iter()
                    .any(|e| matches!(e, WorkEvent::WorkCompleted { .. })),
                "{what}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_passing_project_with_no_change_is_not_verified_work() {
        // Tests that already pass establish nothing about a change: this command is for changing code.
        let fx = fixture("nochange");
        std::fs::write(fx.root.join("src/lib.rs"), RIGHT).unwrap();
        let Some(pax) = pax_for(&fx.root).await else {
            return;
        };
        let script = Script::new(&[test_run(), claim()]);
        let w = go(&fx, pax, &script, LIMITS, &CompleteWhenVerified).await;
        tidy(&w);
        assert_eq!(last_pax(&w).unwrap().status, chip_pax::PaxStatus::Passed);
        assert!(!completed(&w), "{:?}", w.report.outcome);
        // A write of identical content changed nothing, and does not count as a change either.
        let fx = fixture("nochange2");
        std::fs::write(fx.root.join("src/lib.rs"), RIGHT).unwrap();
        let script = Script::new(&[write("src/lib.rs", RIGHT), test_run(), claim()]);
        let w = go(
            &fx,
            pax_for(&fx.root).await.unwrap(),
            &script,
            LIMITS,
            &CompleteWhenVerified,
        )
        .await;
        assert_eq!((w.writes, w.changed_writes), (1, 0));
        assert!(!completed(&w));
    }

    // ---- the adversarial matrix -----------------------------------------------------------------------------------------

    fn field_on_valid_pax_request(extra: &str) -> String {
        format!(r#"{{"decision":"request_capability","capability":"pax.test",{extra}}}"#)
    }

    fn field_on_valid_read(extra: &str) -> String {
        format!(
            r#"{{"decision":"request_capability","capability":"project.read","inputs":{{"path":"src/lib.rs"}},{extra}}}"#
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_rejected_request_executes_observes_records_or_completes() {
        let outside_file =
            std::env::temp_dir().join(format!("chip-work-abs-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&outside_file);
        let big = "a".repeat(chip_project::MAX_WRITE_BYTES + 1);
        let mut cases: Vec<(&str, String)> = vec![
            ("traversal write", write("../outside.txt", "pwned")),
            ("deep traversal write", write("src/../../outside.txt", "pwned")),
            ("traversal read", read("../outside/secret.txt")),
            ("absolute write", write(outside_file.to_str().unwrap(), "pwned")),
            ("absolute read", read("/etc/passwd")),
            ("symlink write", write("linkdir/x.txt", "pwned")),
            ("symlink read", read("linkdir/secret.txt")),
            ("symlinked file write", write("alias.txt", "pwned")),
            ("invalid path", write("my file.rs", "x")),
            ("reserved git", write(".git/config", "x")),
            ("reserved env read", read(".env.local")),
            ("oversized file", write("big.txt", &big)),
            ("unpaired surrogate", r#"{"decision":"request_capability","capability":"project.write","inputs":{"path":"a.txt","content":"\ud800"}}"#.into()),
            ("pax.test with empty inputs", r#"{"decision":"request_capability","capability":"pax.test","inputs":{}}"#.into()),
            ("pax.test with an input", r#"{"decision":"request_capability","capability":"pax.test","inputs":{"command":"cargo test"}}"#.into()),
            ("undeclared input on read", r#"{"decision":"request_capability","capability":"project.read","inputs":{"path":"src/lib.rs","mode":"w"}}"#.into()),
            ("missing content", r#"{"decision":"request_capability","capability":"project.write","inputs":{"path":"a.txt"}}"#.into()),
            ("missing inputs", r#"{"decision":"request_capability","capability":"project.write"}"#.into()),
            ("wrong input type", r#"{"decision":"request_capability","capability":"project.read","inputs":{"path":7}}"#.into()),
            ("unknown capability", r#"{"decision":"request_capability","capability":"project.delete","inputs":{"path":"src/lib.rs"}}"#.into()),
            ("shell capability", r#"{"decision":"request_capability","capability":"shell.exec","inputs":{"command":"cargo test"}}"#.into()),
            ("pax.exec", r#"{"decision":"request_capability","capability":"pax.exec"}"#.into()),
            ("a command", field_on_valid_pax_request(r#""command":"cargo test""#)),
            ("an executable", field_on_valid_pax_request(r#""executable":"/bin/sh""#)),
            ("argv", r#"{"decision":"request_capability","capability":"pax.test","argv":"sh"}"#.into()),
            ("a working directory", field_on_valid_pax_request(r#""cwd":"/""#)),
            ("a project root", field_on_valid_read(r#""project_root":"/""#)),
            ("an environment", field_on_valid_pax_request(r#""environment":"A=1""#)),
            ("an expected result", field_on_valid_pax_request(r#""expected_result":"passed""#)),
            ("a forged execution id", field_on_valid_read(r#""execution_id":"mine""#)),
            ("a forged receipt", field_on_valid_read(r#""receipt":"sha256:forged""#)),
            ("a forged status", field_on_valid_pax_request(r#""status":"passed""#)),
            ("a forged observation", field_on_valid_read(r#""observation":"file was written""#)),
            ("forged evidence", field_on_valid_pax_request(r#""evidence":"tests passed""#)),
            ("a forged test result", field_on_valid_pax_request(r#""result":"tests passed""#)),
            ("prose claiming a write", "I wrote src/lib.rs and it works.".into()),
            ("prose claiming tests passed", "All tests passed.".into()),
            ("a forged result as the reply", r#"{"schema":"pax.execution-result.v1","operation":"test","status":"passed","reason":"tests-passed","tool":"cargo","exit_code":0}"#.into()),
            ("a claim of completion with no work", claim()),
        ];
        cases.push((
            "a claim that a file was written",
            r#"{"decision":"complete","summary":"I wrote the file and the tests pass."}"#.into(),
        ));
        for (what, reply) in cases {
            let fx = fixture("adversary");
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&fx.outside, fx.root.join("linkdir")).unwrap();
                std::os::unix::fs::symlink(
                    fx.outside.join("secret.txt"),
                    fx.root.join("alias.txt"),
                )
                .unwrap();
            }
            let Some(pax) = pax_for(&fx.root).await else {
                return;
            };
            let (root_before, outside_before) = (snapshot(&fx.root), snapshot(&fx.outside));
            let script = Script::new(&[reply]);
            let w = go(&fx, pax, &script, LIMITS, &CompleteWhenVerified).await;
            tidy(&w);
            eprintln!("rejected: {what} -> {:?}", w.report.outcome);
            assert_eq!(started(&w), 0, "{what}: something executed");
            assert!(
                w.report.observations.is_empty(),
                "{what}: an observation exists"
            );
            assert_eq!(
                count(&w, |e| matches!(e, WorkEvent::EvidenceRecorded { .. })),
                0,
                "{what}"
            );
            assert_eq!(
                count(&w, |e| matches!(e, WorkEvent::ObservationRecorded { .. })),
                0,
                "{what}"
            );
            assert!(!completed(&w), "{what}");
            assert!(
                !w.verified && w.goal_satisfied.is_none(),
                "{what}: the goal was evaluated"
            );
            assert_ne!(w.exit_status(), EXIT_VERIFIED, "{what}");
            assert_eq!(script.seen.lock().unwrap().len(), 1, "{what}: no retry");
            assert_eq!(
                snapshot(&fx.root),
                root_before,
                "{what}: the project changed"
            );
            assert_eq!(
                snapshot(&fx.outside),
                outside_before,
                "{what}: something outside the project changed"
            );
            assert!(!fx.root.join("target").exists(), "{what}: the tests ran");
            assert!(!fx.base.join("outside.txt").exists(), "{what}");
            assert!(
                !outside_file.exists(),
                "{what}: a file appeared at an absolute path"
            );
        }
    }

    // ---- bounds ------------------------------------------------------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn every_capability_execution_counts_against_the_one_execution_budget() {
        let fx = fixture("budget");
        let Some(pax) = pax_for(&fx.root).await else {
            return;
        };
        let limits = WorkLimits {
            max_turns: 12,
            max_executions: 3,
        };
        let script = Script::new(&[
            read("src/lib.rs"),
            read("tests/fingerprint.rs"),
            write("notes.txt", "n"),
            read("src/lib.rs"),
            test_run(),
        ]);
        let w = go(&fx, pax, &script, limits, &CompleteWhenVerified).await;
        tidy(&w);
        assert!(
            matches!(
                w.report.outcome,
                WorkOutcome::LimitReached {
                    limit: chip_core::LimitKind::Executions
                }
            ),
            "{:?}",
            w.report.outcome
        );
        assert_eq!(
            started(&w),
            3,
            "reads and writes spend the same budget as tests"
        );
        assert_eq!(w.exit_status(), EXIT_NOT_VERIFIED);
        // The turn budget bounds judgment the same way.
        let fx = fixture("budget2");
        let limits = WorkLimits {
            max_turns: 2,
            max_executions: 8,
        };
        let script = Script::new(&[read("src/lib.rs"), read("tests/fingerprint.rs"), test_run()]);
        let w = go(
            &fx,
            pax_for(&fx.root).await.unwrap(),
            &script,
            limits,
            &CompleteWhenVerified,
        )
        .await;
        assert!(
            matches!(
                w.report.outcome,
                WorkOutcome::LimitReached {
                    limit: chip_core::LimitKind::Turns
                }
            ),
            "{:?}",
            w.report.outcome
        );
        tidy(&w);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_read_or_write_is_real_information_and_changes_nothing() {
        let fx = fixture("realfail");
        let Some(pax) = pax_for(&fx.root).await else {
            return;
        };
        std::fs::write(fx.root.join("blob.bin"), [0xff, 0xfe]).unwrap();
        let before = snapshot(&fx.root);
        let script = Script::new(&[
            read("blob.bin"),
            read("nope.rs"),
            write("nodir/x.rs", "x"),
            claim(),
        ]);
        let w = go(&fx, pax, &script, LIMITS, &CompleteWhenVerified).await;
        tidy(&w);
        assert_eq!(w.utility.failed_observations, 3);
        assert!(
            w.report
                .observations
                .iter()
                .all(|o| o.kind == ObservationKind::ExecutionFailed)
        );
        assert_eq!((w.writes, w.changed_writes), (0, 0));
        assert_eq!(snapshot(&fx.root), before);
        let seen = script.seen.lock().unwrap();
        assert!(
            seen[1].contains("not_utf8")
                && seen[2].contains("not_found")
                && seen[3].contains("parent_missing")
        );
    }

    // ---- PR46: navigating a real multi-file project -------------------------------------------------------------

    const ROWS_LIB: &str = "pub mod parse;\npub mod render;\n";
    const ROWS_PARSE_BUG: &str = "/// Splits one input line into trimmed fields.\npub fn fields(line: &str) -> Vec<String> {\n    line.split(';').map(|f| f.trim().to_string()).collect()\n}\n";
    const ROWS_PARSE_FIXED: &str = "/// Splits one input line into trimmed fields.\npub fn fields(line: &str) -> Vec<String> {\n    line.split(',').map(|f| f.trim().to_string()).collect()\n}\n";
    const ROWS_RENDER_BUG: &str = "/// Renders fields as one table row.\npub fn table_row(fields: &[String]) -> String {\n    fields.join(\",\")\n}\n";
    const ROWS_RENDER_FIXED: &str = "/// Renders fields as one table row.\npub fn table_row(fields: &[String]) -> String {\n    fields.join(\" | \")\n}\n";
    const ROWS_TESTS: &str = "use rowfmt::parse::fields;\nuse rowfmt::render::table_row;\n\n#[test]\nfn fields_are_split_on_commas_and_trimmed() {\n    assert_eq!(fields(\"a, b ,c\"), vec![\"a\", \"b\", \"c\"]);\n}\n\n#[test]\nfn a_row_is_joined_with_pipes() {\n    assert_eq!(table_row(&[\"a\".to_string(), \"b\".to_string()]), \"a | b\");\n}\n\n#[test]\nfn parsing_then_rendering() {\n    assert_eq!(table_row(&fields(\"x,y\")), \"x | y\");\n}\n";

    /// A real multi-file project with a bug in each of two source files.
    fn rows_fixture(tag: &str) -> Fx {
        let fx = fixture(tag);
        std::fs::write(
            fx.root.join("Cargo.toml"),
            format!("[package]\nname = \"rowfmt_{}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nname = \"rowfmt\"\n", tag.replace('-', "_")),
        )
        .unwrap();
        std::fs::remove_file(root_file(&fx, "tests/fingerprint.rs")).unwrap();
        for (path, text) in [
            ("src/lib.rs", ROWS_LIB),
            ("src/parse.rs", ROWS_PARSE_BUG),
            ("src/render.rs", ROWS_RENDER_BUG),
            ("tests/rows.rs", ROWS_TESTS),
        ] {
            std::fs::write(root_file(&fx, path), text).unwrap();
        }
        fx
    }

    fn root_file(fx: &Fx, rel: &str) -> PathBuf {
        fx.root.join(rel)
    }

    fn list_req(path: Option<&str>) -> String {
        let inputs = path.map_or(String::new(), |p| {
            format!(
                r#","inputs":{{"path":{}}}"#,
                serde_json::to_string(p).unwrap()
            )
        });
        format!(r#"{{"decision":"request_capability","capability":"project.list"{inputs}}}"#)
    }

    fn search_req(query: &str, path: Option<&str>) -> String {
        let extra = path.map_or(String::new(), |p| {
            format!(r#","path":{}"#, serde_json::to_string(p).unwrap())
        });
        format!(
            r#"{{"decision":"request_capability","capability":"project.search","inputs":{{"query":{}{extra}}}}}"#,
            serde_json::to_string(query).unwrap()
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn navigation_search_read_change_two_files_and_verify() {
        let fx = rows_fixture("rows");
        let Some(pax) = pax_for(&fx.root).await else {
            return;
        };
        // Search, read, write across two files, read the changed file back, test: the model is
        // never told a path, and visits a capability again with a different input after a failure.
        let script = Script::new(&[
            list_req(None),
            search_req("table_row", None),
            read("src/nope.rs"), // a wrong guess: reality says it is not there
            read("tests/rows.rs"),
            read("src/parse.rs"),
            write("src/parse.rs", ROWS_PARSE_FIXED),
            read("src/render.rs"),
            write("src/render.rs", ROWS_RENDER_FIXED),
            read("src/parse.rs"), // the changed file, as it now is
            test_run(),
        ]);
        let limits = WorkLimits {
            max_turns: 14,
            max_executions: 12,
        };
        let w = go(&fx, pax, &script, limits, &CompleteWhenVerified).await;
        tidy(&w);
        assert!(completed(&w), "{:?}", w.report.outcome);
        assert!(w.verified);
        assert_eq!(
            (
                w.lists(),
                w.searches(),
                w.reads(),
                w.writes,
                w.changed_writes,
                w.pax_executions()
            ),
            (1, 1, 5, 2, 2, 1)
        );
        assert_eq!(w.utility.executions, 10);
        assert_eq!(w.utility.failed_observations, 1);
        assert!(w.utility.recoveries >= 1);
        assert_eq!(w.useful_writes(), 2);
        assert_eq!(
            std::fs::read_to_string(fx.root.join("src/parse.rs")).unwrap(),
            ROWS_PARSE_FIXED
        );
        assert_eq!(
            std::fs::read_to_string(fx.root.join("src/render.rs")).unwrap(),
            ROWS_RENDER_FIXED
        );
        assert_eq!(
            std::fs::read_to_string(fx.root.join("tests/rows.rs")).unwrap(),
            ROWS_TESTS,
            "the tests were not touched"
        );
        assert_eq!(w.report.measurement().model_calls, 10);
        // The model was shown real navigation results, and the changed file as it then was.
        let seen = script.seen.lock().unwrap();
        assert!(
            seen[1].contains("dir src") && seen[1].contains("dir tests"),
            "the listing did not reach the model"
        );
        assert!(
            seen[2].contains("src/render.rs:2: pub fn table_row"),
            "the search result did not reach the model"
        );
        assert!(
            seen[9].contains("split(',')"),
            "the changed file was not shown back"
        );
        assert!(
            seen.iter().all(|m| !m.contains(fx.root.to_str().unwrap())),
            "a host path reached the model"
        );
        // A note that an invocation fell short is about that invocation: reading another file is
        // not ruled out because one read was.
        let after_wrong_guess = &seen[3];
        assert!(
            after_wrong_guess
                .contains(r#"capability project.read (path="src/nope.rs"): its execution failed"#),
            "{after_wrong_guess}"
        );
        assert!(
            !after_wrong_guess.contains("capability project.read: "),
            "{after_wrong_guess}"
        );
        // The utility numbers for the report.
        let json: serde_json::Value =
            serde_json::from_str(&render_json(&w, &resolved_pax())).unwrap();
        assert_eq!(
            (
                json["lists"].as_u64(),
                json["searches"].as_u64(),
                json["reads"].as_u64(),
                json["writes"].as_u64(),
                json["tests"].as_u64()
            ),
            (Some(1), Some(1), Some(5), Some(2), Some(1))
        );
        assert_eq!(
            json["useful_work_per_model_call"].as_f64().unwrap(),
            1.0 / 10.0
        );
        assert_eq!(
            json["useful_work_per_execution"].as_f64().unwrap(),
            1.0 / 10.0
        );
        assert_eq!(json["audit"]["navigation_mismatch"], 0);
        assert_eq!(json["audit"]["host_path_leak"], 0);
        assert_eq!(json["audit"]["forged_observations"], 0);
        assert!(
            render_human(&w, &resolved_pax())
                .contains("Lists: 1  Searches: 1  Reads: 5  Writes: 2 (2 changed)  Tests: 1")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unverified_navigation_and_writes_are_not_useful_work() {
        let fx = rows_fixture("rows-unverified");
        let Some(pax) = pax_for(&fx.root).await else {
            return;
        };
        // Plenty of activity, a real change, and a failing test run: nothing verified.
        let script = Script::new(&[
            list_req(Some("src")),
            search_req("fields", None),
            read("src/parse.rs"),
            write("src/parse.rs", ROWS_PARSE_FIXED),
            test_run(),
            claim(),
        ]);
        let w = go(&fx, pax, &script, LIMITS, &CompleteWhenVerified).await;
        tidy(&w);
        assert!(!completed(&w) && !w.verified);
        assert_eq!(last_pax(&w).unwrap().status, chip_pax::PaxStatus::Failed);
        assert_eq!((w.writes, w.changed_writes, w.useful_writes()), (1, 1, 0));
        assert_eq!(w.utility.verified_outputs, 0);
        assert_eq!(w.utility.work_per_model_call(), Some(0.0));
        assert_eq!(w.utility.work_per_execution(), Some(0.0));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_rejected_navigation_request_executes_observes_records_or_completes() {
        let abs_root = std::env::temp_dir()
            .join(format!("chip-work-{}-nav-adversary", std::process::id()))
            .join("project");
        let long_query = "q".repeat(300);
        let huge_query = "q".repeat(2000);
        let cases: Vec<(&str, String)> = vec![
            ("list traversal", list_req(Some("../outside"))),
            ("list deep traversal", list_req(Some("src/../.."))),
            ("list absolute", list_req(Some("/etc"))),
            ("list the absolute root", list_req(Some(abs_root.to_str().unwrap()))),
            ("list reserved git", list_req(Some(".git"))),
            ("list reserved env", list_req(Some(".env"))),
            ("list symlink", list_req(Some("linkdir"))),
            ("list through a symlink", list_req(Some("linkdir/sub"))),
            ("list empty path", list_req(Some(""))),
            ("list malformed path", list_req(Some("src//x"))),
            ("list an undeclared field", r#"{"decision":"request_capability","capability":"project.list","inputs":{"path":"src","recursive":"yes"}}"#.into()),
            ("list a command", r#"{"decision":"request_capability","capability":"project.list","inputs":{"command":"ls -la /"}}"#.into()),
            ("list integer path", r#"{"decision":"request_capability","capability":"project.list","inputs":{"path":7}}"#.into()),
            ("search traversal", search_req("x", Some("../outside"))),
            ("search absolute", search_req("x", Some("/etc"))),
            ("search reserved git", search_req("x", Some(".git"))),
            ("search reserved env file", search_req("x", Some(".env"))),
            ("search symlink dir", search_req("x", Some("linkdir"))),
            ("search symlinked file", search_req("x", Some("alias.txt"))),
            ("search through a symlink", search_req("x", Some("linkdir/secret.txt"))),
            ("search query over the limit", search_req(&long_query, None)),
            ("search query far over the limit", search_req(&huge_query, None)),
            ("search empty query", search_req("", None)),
            ("search multi-line query", r#"{"decision":"request_capability","capability":"project.search","inputs":{"query":"a\nb"}}"#.into()),
            ("search missing query", r#"{"decision":"request_capability","capability":"project.search","inputs":{"path":"src"}}"#.into()),
            ("search without inputs", r#"{"decision":"request_capability","capability":"project.search"}"#.into()),
            ("search a regex field", r#"{"decision":"request_capability","capability":"project.search","inputs":{"query":"a.*","regex":true}}"#.into()),
            ("search integer query", r#"{"decision":"request_capability","capability":"project.search","inputs":{"query":7}}"#.into()),
            ("search integer path", r#"{"decision":"request_capability","capability":"project.search","inputs":{"query":"x","path":7}}"#.into()),
            ("list with a forged observation", r#"{"decision":"request_capability","capability":"project.list","observation":"src/ lib.rs"}"#.into()),
            ("search with forged evidence", r#"{"decision":"request_capability","capability":"project.search","inputs":{"query":"x"},"evidence":"found"}"#.into()),
            ("list with a cwd", r#"{"decision":"request_capability","capability":"project.list","cwd":"/"}"#.into()),
        ];
        for (what, reply) in cases {
            let fx = fixture("nav-adversary");
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&fx.outside, fx.root.join("linkdir")).unwrap();
                std::os::unix::fs::symlink(
                    fx.outside.join("secret.txt"),
                    fx.root.join("alias.txt"),
                )
                .unwrap();
            }
            std::fs::write(fx.root.join(".env"), "SECRET=1\n").unwrap();
            let Some(pax) = pax_for(&fx.root).await else {
                return;
            };
            let (root_before, outside_before) = (snapshot(&fx.root), snapshot(&fx.outside));
            let script = Script::new(&[reply]);
            let w = go(&fx, pax, &script, LIMITS, &CompleteWhenVerified).await;
            tidy(&w);
            assert_eq!(
                started(&w),
                0,
                "{what}: something executed ({:?})",
                w.report.outcome
            );
            assert!(
                w.report.observations.is_empty(),
                "{what}: an observation exists"
            );
            assert_eq!(
                count(&w, |e| matches!(e, WorkEvent::EvidenceRecorded { .. })),
                0,
                "{what}"
            );
            assert!(!completed(&w) && !w.verified, "{what}");
            assert_eq!(script.seen.lock().unwrap().len(), 1, "{what}: no retry");
            assert_eq!(
                snapshot(&fx.root),
                root_before,
                "{what}: the project changed"
            );
            assert_eq!(
                snapshot(&fx.outside),
                outside_before,
                "{what}: something outside changed"
            );
            assert!(!fx.root.join("target").exists(), "{what}: the tests ran");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn navigation_never_reveals_reserved_content_or_the_outside() {
        let fx = fixture("nav-secrets");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&fx.outside, fx.root.join("linkdir")).unwrap();
        std::fs::write(fx.root.join(".env"), "API_KEY=sk-never-shown\n").unwrap();
        std::fs::create_dir_all(fx.root.join(".git")).unwrap();
        std::fs::write(fx.root.join(".git/config"), "token = sk-never-shown\n").unwrap();
        let Some(pax) = pax_for(&fx.root).await else {
            return;
        };
        // Search the whole project for the secret, list the root, then look for the outside's secret.
        let script = Script::new(&[
            search_req("sk-never-shown", None),
            list_req(None),
            search_req("outside secret", None),
            claim(),
        ]);
        let w = go(&fx, pax, &script, LIMITS, &CompleteWhenVerified).await;
        tidy(&w);
        for o in &w.report.observations {
            // What came back: no match rows and no entries that are the secrets or the outside.
            let text = o.output.as_deref().unwrap();
            let returned: String = text.lines().skip(2).collect::<Vec<_>>().join("\n");
            assert!(
                !returned.contains("sk-never-shown") && !returned.contains("outside secret"),
                "{text}"
            );
            assert!(
                !returned.contains(".env")
                    && !returned.contains(".git")
                    && !returned.contains("linkdir"),
                "{text}"
            );
        }
        assert_eq!((w.searches(), w.lists()), (2, 1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_real_capability_set_is_shown_to_the_model_exactly_as_it_is_enforced() {
        let fx = fixture("contract");
        let set = CapabilitySet::new()
            .with(Arc::new(ProjectExecutor::new(&fx.root)))
            .with(Arc::new(PaxExecutor::new(&fx.root)));
        let descriptors = set.capabilities().await.unwrap();
        let offered: Vec<chip_core::Capability> = descriptors
            .iter()
            .cloned()
            .map(|descriptor| chip_core::Capability {
                descriptor,
                availability: chip_core::CapabilityAvailability::Available,
            })
            .collect();
        let q = chip_core::WorkDecisionBoundary::question(&ModelDecisionBoundary, &offered);
        assert_eq!(
            q,
            chip_core::WorkDecisionBoundary::question(&ModelDecisionBoundary, &offered),
            "deterministic"
        );
        // pax.test declares no inputs: it is shown as taking none, with no `inputs` field in its form.
        assert!(q.contains(r#"pax.test takes no inputs: {"decision":"request_capability","capability":"pax.test"} (no "inputs" field)"#), "{q}");
        assert!(q.contains(r#"{"decision":"request_capability","capability":"pax.test","inputs":{}} is invalid"#), "{q}");
        // project.read declares a required path; project.list only an optional one.
        assert!(q.contains(r#"project.read takes inputs (path required): {"decision":"request_capability","capability":"project.read","inputs":{"path":<string|integer|boolean>}}"#), "{q}");
        assert!(
            q.contains(r#"project.write takes inputs (path required, content required)"#),
            "{q}"
        );
        assert!(
            q.contains(r#"project.search takes inputs (query required, path optional)"#),
            "{q}"
        );
        assert!(
            q.contains(r#"project.list takes inputs (path optional)"#),
            "{q}"
        );
        // And every capability is described as its descriptor declares it.
        for d in &descriptors {
            let id = d.id.as_str();
            let takes_none = q.contains(&format!("{id} takes no inputs"));
            let takes_some = q.contains(&format!("{id} takes inputs ("));
            assert_eq!(
                (takes_none, takes_some),
                (d.inputs.is_empty(), !d.inputs.is_empty()),
                "{id}"
            );
        }
        // The task prompts are untouched by any of this: the goal text carries no protocol advice.
        let goal = goal_text(GOAL);
        assert!(!goal.contains("inputs") && !goal.contains("{}"), "{goal}");
    }

    #[test]
    fn the_goal_text_states_the_rules_chip_applies() {
        let g = goal_text("  Fix the bug.  ");
        assert!(g.starts_with("Fix the bug. Inspect and change"));
        assert!(g.contains("Chip decides completion") && g.contains("pax.test"));
    }
}
