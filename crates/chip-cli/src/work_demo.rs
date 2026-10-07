//! `chip-cli --test-work [scenario]` and `--test-real-work`: the bounded autonomous loop, end to end.
//!
//! One `run_work` call makes every decision; this module never starts a second turn. The model is
//! a deterministic stand-in, so nothing here needs credentials. `--test-real-work` uses real
//! Compute and exits with status 3 (skipped) when Compute is not available.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chip_core::{
    Agent, Capability, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityProvider, CapabilityRequest, DecisionError, DecisionSource, EvidenceState,
    ExecutionError, ExecutionId, ExecutionObserver, ExecutionRequest, ExecutionResult,
    ExecutionStatus, Executor, LimitKind, LocalReasoningResult, LocalWorkPolicy,
    ModelDecisionBoundary, ObservationKind, ScriptedPolicy, TerminalState, TestLocalReasoner,
    WorkDecision, WorkDecisionBoundary, WorkEvent, WorkGoal, WorkId, WorkLimits, WorkMeasurement,
    WorkOutcome, WorkReport, WorkSpec, WorkView, verify_trajectory,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

const CAPABILITY: &str = "compute.selftest";

/// Counts calls; answers every escalation with the same text.
struct StandInModel(Arc<AtomicUsize>, &'static str);

#[async_trait::async_trait]
impl ModelProvider for StandInModel {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse, FxError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ModelResponse::new("work-model", self.1, Usage::new(1, 1)))
    }
}

/// Plays back a fixed list of decisions, one per escalation.
struct ModelDecisions(Vec<WorkDecision>, AtomicUsize);

impl WorkDecisionBoundary for ModelDecisions {
    fn interpret(
        &self,
        _r: &ModelResponse,
        _c: &[Capability],
    ) -> Result<WorkDecision, DecisionError> {
        let i = self.1.fetch_add(1, Ordering::SeqCst);
        self.0
            .get(i)
            .cloned()
            .ok_or_else(|| DecisionError::InvalidDecision("no more scripted decisions".into()))
    }
}

struct CountingExecutor(Arc<dyn Executor>, Arc<AtomicUsize>);

#[async_trait::async_trait]
impl Executor for CountingExecutor {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0.execute(request).await
    }
}

/// Deterministic executor: a fixed status and a fixed receipt.
struct Fixed(ExecutionStatus);

#[async_trait::async_trait]
impl Executor for Fixed {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        Ok(ExecutionResult {
            id: request.id,
            status: self.0,
            output: match self.0 {
                ExecutionStatus::Success => "self test passed".into(),
                _ => "self test failed".into(),
            },
            receipt_id: Some("sha256:demo-receipt".into()),
        })
    }
}

struct SelfTestCapability;

#[async_trait::async_trait]
impl CapabilityProvider for SelfTestCapability {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        let mut descriptor = CapabilityDescriptor::new(
            CapabilityId::new(CAPABILITY)?,
            "Self test",
            "Deterministic self test",
        );
        descriptor.inputs.push(chip_core::CapabilityInput {
            name: "n".into(),
            description: "variant".into(),
            required: false,
        });
        Ok(vec![descriptor])
    }

    async fn availability(&self, _id: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
    }
}

fn request(execution: &str) -> CapabilityRequest {
    CapabilityRequest::new(
        ExecutionId::new(execution),
        CapabilityId::new(CAPABILITY).unwrap(),
    )
}

/// Decides from what has been observed: request the self test; complete if it passed, block if it
/// failed. The decisions are the policy's reaction to the observation, not a fabricated outcome.
struct ReactToObservation;

impl LocalWorkPolicy for ReactToObservation {
    fn propose(&self, view: &WorkView<'_>) -> Option<WorkDecision> {
        match view.observations.last() {
            None => Some(WorkDecision::RequestCapability(request("work-1"))),
            Some(o) if o.kind == ObservationKind::ExecutionCompleted => {
                Some(WorkDecision::Complete {
                    summary: format!(
                        "the self test passed (receipt {})",
                        o.receipt_id.as_deref().unwrap_or("none")
                    ),
                })
            }
            Some(o) if o.kind == ObservationKind::ExecutionFailed => Some(WorkDecision::Block {
                reason: "the self test failed".into(),
            }),
            Some(_) => Some(WorkDecision::Escalate {
                reason: "the execution was cancelled".into(),
            }),
        }
    }
}

/// Always asks for another distinct execution (a different input each time).
struct Insatiable;

impl LocalWorkPolicy for Insatiable {
    fn propose(&self, view: &WorkView<'_>) -> Option<WorkDecision> {
        Some(WorkDecision::RequestCapability(
            request(&format!("work-{}", view.turn))
                .with_input("n", chip_core::InputValue::Integer(view.turn as i64)),
        ))
    }
}

/// Always asks for the very same thing, so evidence answers every time after the first.
struct Repeating;

impl LocalWorkPolicy for Repeating {
    fn propose(&self, view: &WorkView<'_>) -> Option<WorkDecision> {
        Some(WorkDecision::RequestCapability(request(&format!(
            "work-{}",
            view.turn
        ))))
    }
}

fn describe(event: &WorkEvent) -> String {
    match event {
        WorkEvent::WorkStarted { goal, limits, .. } => format!(
            "WorkStarted (goal: {goal}; max turns {}, max executions {})",
            limits.max_turns, limits.max_executions
        ),
        WorkEvent::Capability(e) => format!("{e:?}"),
        WorkEvent::DecisionStarted { turn, .. } => format!("DecisionStarted (turn {})", turn + 1),
        WorkEvent::LocalDecision { decision, .. } => format!("LocalDecision: {decision}"),
        WorkEvent::ModelEscalation {
            reason, context, ..
        } => format!(
            "ModelEscalation: {reason} (context {} bytes, {} observations, {} decisions)",
            context.bytes, context.observations, context.decisions
        ),
        WorkEvent::ModelCalled {
            usage, succeeded, ..
        } => match (succeeded, usage) {
            (false, _) => "ModelCalled (failed)".to_string(),
            (true, Some(u)) => format!(
                "ModelCalled (tokens: prompt {}, completion {})",
                u.prompt_tokens, u.completion_tokens
            ),
            (true, None) => "ModelCalled (usage not reported)".to_string(),
        },
        WorkEvent::DecisionMade { decision, .. } => format!("DecisionMade: {decision}"),
        WorkEvent::CapabilityRequested { capability, .. } => {
            format!("CapabilityRequested: {capability}")
        }
        WorkEvent::Execution(e) => format!("{e:?}"),
        WorkEvent::ObservationRecorded {
            kind, receipt_id, ..
        } => format!(
            "ObservationRecorded: {} (receipt {})",
            kind.as_str(),
            receipt_id.as_deref().unwrap_or("none")
        ),
        WorkEvent::EvidenceRecorded { capability, .. } => {
            format!("EvidenceRecorded: {capability}")
        }
        WorkEvent::EvidenceReused {
            capability,
            receipt_id,
            ..
        } => format!(
            "EvidenceReused: {capability} (receipt {})",
            receipt_id.as_deref().unwrap_or("none")
        ),
        WorkEvent::WorkCompleted { .. } => "WorkCompleted".to_string(),
        WorkEvent::WorkEscalated { reason, .. } => format!("WorkEscalated: {reason}"),
        WorkEvent::WorkBlocked { reason, .. } => format!("WorkBlocked: {reason}"),
        WorkEvent::WorkLimitReached { limit, .. } => {
            format!("WorkLimitReached: {}", limit.name())
        }
        WorkEvent::WorkFailed { reason, .. } => format!("WorkFailed: {reason}"),
    }
}

fn ms(d: Duration) -> String {
    format!("{:.3}", d.as_secs_f64() * 1000.0)
}

fn print_report(title: &str, report: &WorkReport, model_calls: usize, execute_calls: usize) {
    println!("{title}\n");
    println!("Trajectory:");
    for (i, event) in report.events.iter().enumerate() {
        println!("  {:>2}. {}", i + 1, describe(event));
    }
    let s = &report.summary;
    let m = report.measurement();
    println!("\nSummary:");
    println!("  turns: {}", s.turns);
    println!(
        "  executions: {} (executor calls observed: {execute_calls})",
        s.executions
    );
    println!("  observations: {}", s.observations);
    println!("  evidence_hits: {}", s.evidence_hits);
    println!("  local_decisions: {}", s.local_decisions);
    println!(
        "  model_escalations: {} (model calls observed: {model_calls})",
        s.model_escalations
    );
    println!("  context_bytes: {}", s.context_bytes);
    println!(
        "  tokens (provider-reported): prompt {}, completion {}",
        s.prompt_tokens, s.completion_tokens
    );
    println!("  elapsed_time: {:?}", s.elapsed);
    println!("  model_latency: {} ms", ms(m.model_latency));
    println!("  compute_latency: {} ms", ms(m.compute_latency));
    println!(
        "  local_decision_latency: {} ms",
        ms(m.local_decision_latency)
    );
    println!("  terminal_state: {}", s.terminal_state.name());
    match &report.outcome {
        WorkOutcome::Completed { summary } => println!("  outcome: {summary}"),
        WorkOutcome::Escalated { reason }
        | WorkOutcome::Blocked { reason }
        | WorkOutcome::Failed { reason } => println!("  outcome: {reason}"),
        WorkOutcome::LimitReached { limit } => {
            println!("  outcome: {} limit reached", limit.name())
        }
    }
}

/// The measurement as stable JSON: a fixed key order, no prompts, no secrets, no identifiers, no
/// timestamps. Latencies are the only non-deterministic values, and carry an `_ms` suffix.
pub fn measurement_json(workload: &str, m: &WorkMeasurement) -> String {
    format!(
        "{{\"workload\":\"{workload}\",\"outcome\":\"{}\",\"turns\":{},\"executions\":{},\"observations\":{},\"evidence_hits\":{},\"local_decisions\":{},\"model_escalations\":{},\"context_bytes\":{},\"context_chars\":{},\"model_calls\":{},\"model_tokens\":{},\"model_latency_ms\":{},\"compute_latency_ms\":{},\"local_decision_latency_ms\":{},\"total_latency_ms\":{}}}",
        m.terminal_state().name(),
        m.turns,
        m.executions,
        m.observations,
        m.evidence_hits,
        m.local_decisions,
        m.model_escalations,
        m.context_bytes,
        m.context_chars,
        m.model_calls,
        m.model_tokens.map_or("null".to_string(), |t| t.to_string()),
        ms(m.model_latency),
        ms(m.compute_latency),
        ms(m.local_decision_latency),
        ms(m.total_latency),
    )
}

/// The structural trajectory, turn by turn. No prompt text.
pub fn render_trace(report: &WorkReport) -> String {
    let mut out = String::new();
    for t in report.trace() {
        out.push_str(&format!("TURN {}\n", t.turn + 1));
        out.push_str(&format!("  decision: {}\n", t.decision.unwrap_or("none")));
        if let Some(c) = &t.capability {
            out.push_str(&format!("  capability: {c}\n"));
        }
        if let Some(source) = t.source {
            out.push_str(&format!(
                "  source: {}\n",
                if source == DecisionSource::Local {
                    "local"
                } else {
                    "model"
                }
            ));
        }
        if t.decision == Some("RequestCapability") {
            out.push_str(&format!(
                "  execution: {}\n",
                if t.executed {
                    "yes"
                } else if t.evidence_reused {
                    "no (evidence reused)"
                } else {
                    "no"
                }
            ));
        }
        if let Some(receipt) = t.receipt_present {
            out.push_str(&format!(
                "  receipt: {}\n",
                if receipt { "present" } else { "absent" }
            ));
        }
        if t.decision == Some("RequestCapability") {
            out.push_str(&format!(
                "  observation: {}\n",
                if t.observed { "present" } else { "none" }
            ));
            if t.evidence_recorded {
                out.push_str("  evidence: recorded\n");
            }
        }
        if let Some(c) = t.context {
            out.push_str("  context:\n");
            out.push_str(&format!("    observations: {}\n", c.observations));
            out.push_str(&format!("    evidence: {}\n", c.evidence_items));
            out.push_str(&format!("    decisions: {}\n", c.decisions));
            out.push_str(&format!("    ruled_out: {}\n", c.ruled_out));
            out.push_str(&format!("    bytes: {}\n", c.bytes));
            out.push_str(&format!(
                "  model: {}\n",
                if t.model_called {
                    "called"
                } else {
                    "not called"
                }
            ));
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "OUTCOME\n  {}\n",
        match &report.outcome {
            WorkOutcome::LimitReached { limit } => format!("limit_reached ({})", limit.name()),
            other => other.terminal_state().name().to_string(),
        }
    ));
    out
}

struct Scenario {
    title: &'static str,
    goal: &'static str,
    status: ExecutionStatus,
    limits: WorkLimits,
    policy: Box<dyn LocalWorkPolicy>,
    model_decisions: Vec<WorkDecision>,
    /// Interpret the stand-in model's reply under the real `chip.work-decision.v1` contract,
    /// instead of playing back scripted decisions.
    contract_reply: Option<&'static str>,
    /// Establish valid evidence with one real prior execution, outside the measured work.
    seed_evidence: bool,
    expect: fn(&WorkReport) -> bool,
}

fn scenario(name: &str) -> Option<Scenario> {
    let goal = "Perform the self test and determine whether it succeeded";
    let limits = WorkLimits {
        max_turns: 6,
        max_executions: 3,
    };
    Some(match name {
        "completion" => Scenario {
            title: "Bounded autonomous work: completion",
            goal,
            status: ExecutionStatus::Success,
            limits,
            policy: Box::new(ReactToObservation),
            model_decisions: vec![],
            contract_reply: None,
            seed_evidence: false,
            expect: |r| {
                matches!(r.outcome, WorkOutcome::Completed { .. })
                    && r.summary.executions == 1
                    && r.summary.turns == 2
            },
        },
        "failure" => Scenario {
            title: "Bounded autonomous work: the first execution fails",
            goal,
            status: ExecutionStatus::Failure,
            limits,
            policy: Box::new(ReactToObservation),
            model_decisions: vec![],
            contract_reply: None,
            seed_evidence: false,
            expect: |r| {
                matches!(r.outcome, WorkOutcome::Blocked { .. })
                    && r.summary.executions == 1
                    && r.observations[0].kind == ObservationKind::ExecutionFailed
            },
        },
        "limit" => Scenario {
            title: "Bounded autonomous work: a workload that never stops asking",
            goal,
            status: ExecutionStatus::Success,
            limits: WorkLimits {
                max_turns: 6,
                max_executions: 4,
            },
            policy: Box::new(Insatiable),
            model_decisions: vec![],
            contract_reply: None,
            seed_evidence: false,
            expect: |r| {
                r.outcome
                    == WorkOutcome::LimitReached {
                        limit: LimitKind::Executions,
                    }
                    && r.summary.executions == 4
                    && r.summary.turns == 5
            },
        },
        "turn-limit" => Scenario {
            title: "Bounded autonomous work: asking for the same thing forever, answered by evidence",
            goal,
            status: ExecutionStatus::Success,
            limits: WorkLimits {
                max_turns: 5,
                max_executions: 3,
            },
            policy: Box::new(Repeating),
            model_decisions: vec![],
            contract_reply: None,
            seed_evidence: false,
            expect: |r| {
                r.outcome
                    == WorkOutcome::LimitReached {
                        limit: LimitKind::Turns,
                    }
                    && r.summary.turns == 5
                    && r.summary.executions == 1
                    && r.summary.evidence_hits == 4
            },
        },
        "evidence" => Scenario {
            title: "Bounded autonomous work: existing valid evidence, so nothing is executed",
            goal,
            status: ExecutionStatus::Success,
            limits,
            policy: Box::new(ScriptedPolicy::new(vec![
                Some(WorkDecision::RequestCapability(request("again"))),
                Some(WorkDecision::Complete {
                    summary: "the existing evidence answered the request".into(),
                }),
            ])),
            model_decisions: vec![],
            contract_reply: None,
            seed_evidence: true,
            expect: |r| {
                matches!(r.outcome, WorkOutcome::Completed { .. })
                    && r.summary.executions == 0
                    && r.summary.evidence_hits == 1
                    && r.summary.model_escalations == 0
            },
        },
        "evidence-in-loop" => Scenario {
            title: "Bounded autonomous work: evidence established and reused inside one workload",
            goal,
            status: ExecutionStatus::Success,
            limits,
            policy: Box::new(ScriptedPolicy::new(vec![
                Some(WorkDecision::RequestCapability(request("first"))),
                Some(WorkDecision::RequestCapability(request("second"))),
                Some(WorkDecision::Complete {
                    summary: "the evidence answered the second request".into(),
                }),
            ])),
            model_decisions: vec![],
            contract_reply: None,
            seed_evidence: false,
            expect: |r| {
                matches!(r.outcome, WorkOutcome::Completed { .. })
                    && r.summary.executions == 1
                    && r.summary.evidence_hits == 1
            },
        },
        "escalation" => Scenario {
            title: "Bounded autonomous work: local reasoning cannot decide, so the model is asked once",
            goal,
            status: ExecutionStatus::Success,
            limits,
            policy: Box::new(ScriptedPolicy::new(vec![
                None,
                Some(WorkDecision::Complete {
                    summary: "completed after the model's request ran".into(),
                }),
            ])),
            model_decisions: vec![WorkDecision::RequestCapability(request("asked-by-model"))],
            contract_reply: None,
            seed_evidence: false,
            expect: |r| {
                matches!(r.outcome, WorkOutcome::Completed { .. })
                    && r.summary.model_escalations == 1
                    && r.summary.executions == 1
            },
        },
        "model-contract" => Scenario {
            title: "Bounded autonomous work: the model's reply is read under the work-decision contract",
            goal,
            status: ExecutionStatus::Success,
            limits,
            policy: Box::new(ScriptedPolicy::new(vec![
                None,
                Some(WorkDecision::Complete {
                    summary: "completed after the model's request ran".into(),
                }),
            ])),
            model_decisions: vec![],
            contract_reply: Some(
                r#"{"decision":"request_capability","capability":"compute.selftest"}"#,
            ),
            seed_evidence: false,
            expect: |r| {
                matches!(r.outcome, WorkOutcome::Completed { .. })
                    && r.summary.model_escalations == 1
                    && r.summary.executions == 1
            },
        },
        _ => return None,
    })
}

/// The fixed baseline suite that future optimization is measured against.
pub const CANONICAL: [&str; 5] = ["completion", "failure", "evidence", "limit", "escalation"];

pub const SCENARIOS: [&str; 8] = [
    "completion",
    "failure",
    "evidence",
    "limit",
    "escalation",
    "turn-limit",
    "evidence-in-loop",
    "model-contract",
];

struct Finished {
    title: &'static str,
    report: WorkReport,
    limits: WorkLimits,
    model_calls: usize,
    executor_calls: usize,
    reached_expected: bool,
}

/// Runs one scenario to its end. The trajectory invariants are checked for every run.
async fn run_scenario(name: &str) -> Option<Finished> {
    let s = scenario(name)?;
    let model_calls = Arc::new(AtomicUsize::new(0));
    let execute_calls = Arc::new(AtomicUsize::new(0));
    let reasoner = TestLocalReasoner::default().on(
        EvidenceState::Unknown,
        LocalReasoningResult::Continue {
            rationale: "the demo continues".into(),
        },
    );
    let agent = Agent::new(Arc::new(StandInModel(
        model_calls.clone(),
        s.contract_reply.unwrap_or("run the self test"),
    )))
    .with_capabilities(Arc::new(SelfTestCapability))
    .with_executor(Arc::new(CountingExecutor(
        Arc::new(Fixed(s.status)),
        execute_calls.clone(),
    )))
    .with_observer(Arc::new(ExecutionObserver))
    .with_local_reasoner(Arc::new(reasoner));
    if s.seed_evidence {
        // A real prior execution, not part of the measured work.
        agent
            .obtain_evidence(&request("seed"))
            .await
            .expect("seeding evidence");
    }
    let before = execute_calls.load(Ordering::SeqCst);
    let boundary: Box<dyn WorkDecisionBoundary> = if s.contract_reply.is_some() {
        Box::new(ModelDecisionBoundary)
    } else {
        Box::new(ModelDecisions(s.model_decisions, AtomicUsize::new(0)))
    };
    let spec = WorkSpec::new(WorkId::new(format!("demo-{name}")), WorkGoal::new(s.goal))
        .with_limits(s.limits);
    let report = agent
        .run_work(&spec, s.policy.as_ref(), boundary.as_ref())
        .await;
    let violations = verify_trajectory(&report.events, &s.limits);
    for v in &violations {
        eprintln!("invariant violated: {v}");
    }
    let reached_expected = violations.is_empty() && (s.expect)(&report);
    Some(Finished {
        title: s.title,
        limits: s.limits,
        model_calls: model_calls.load(Ordering::SeqCst),
        executor_calls: execute_calls.load(Ordering::SeqCst) - before,
        reached_expected,
        report,
    })
}

/// `--test-work [scenario] [--json] [--trace]`. Returns the process exit code.
pub async fn test_work(args: &[String]) -> i32 {
    let json = args.iter().any(|a| a == "--json");
    let trace = args.iter().any(|a| a == "--trace");
    let name = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(String::as_str)
        .unwrap_or("completion");
    let Some(run) = run_scenario(name).await else {
        eprintln!(
            "unknown scenario {name:?}; choose one of: {}",
            SCENARIOS.join(", ")
        );
        return 2;
    };
    if json {
        println!("{}", measurement_json(name, &run.report.measurement()));
    } else if trace {
        print!("{}", render_trace(&run.report));
    } else {
        print_report(run.title, &run.report, run.model_calls, run.executor_calls);
    }
    let bounded = run.report.summary.turns <= run.limits.max_turns
        && run.report.summary.executions <= run.limits.max_executions;
    if bounded && run.reached_expected {
        if !json && !trace {
            println!(
                "\nExpected terminal state reached: {}",
                run.report.summary.terminal_state.name()
            );
        }
        0
    } else {
        eprintln!("\nFAILED: the workload did not reach its expected terminal state");
        1
    }
}

/// `--benchmark-work [--json]`: the canonical suite, as a table or a JSON array.
pub async fn benchmark_work(args: &[String]) -> i32 {
    let json = args.iter().any(|a| a == "--json");
    let mut rows = Vec::new();
    let mut ok = true;
    for name in CANONICAL {
        let run = run_scenario(name).await.expect("canonical workloads exist");
        ok &= run.reached_expected
            && run.report.summary.turns <= run.limits.max_turns
            && run.report.summary.executions <= run.limits.max_executions
            && run.model_calls == run.report.summary.model_escalations
            && run.executor_calls == run.report.summary.executions;
        rows.push((name, run.report.measurement()));
    }
    if json {
        let items: Vec<String> = rows.iter().map(|(n, m)| measurement_json(n, m)).collect();
        println!("[{}]", items.join(",\n "));
    } else {
        println!(
            "{:<12} {:>5} {:>5} {:>5} {:>5} {:>8} {:>8}  {:<14} {:>9} {:>10} {:>9}",
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
            "Total ms"
        );
        for (name, m) in &rows {
            println!(
                "{:<12} {:>5} {:>5} {:>5} {:>5} {:>8} {:>8}  {:<14} {:>9} {:>10} {:>9}",
                name,
                m.turns,
                m.executions,
                m.local_decisions,
                m.model_escalations,
                m.evidence_hits,
                m.context_bytes,
                m.terminal_state().name(),
                ms(m.model_latency),
                ms(m.compute_latency),
                ms(m.total_latency),
            );
        }
        println!(
            "\nEvery run satisfied the trajectory invariants (bounds, one decision and at most one execution per turn,"
        );
        println!(
            "evidence reuse without execution, observations only from executions, one model call per escalation)."
        );
    }
    if ok {
        0
    } else {
        eprintln!("FAILED: a canonical workload did not behave as specified");
        1
    }
}

/// Real Compute, never a fallback. Exit 3 means skipped.
pub async fn test_real_work(args: &[String]) -> i32 {
    let json = args.iter().any(|a| a == "--json");
    let compute = Arc::new(chip_compute::ComputeExecutor::new());
    let model_calls = Arc::new(AtomicUsize::new(0));
    let execute_calls = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(Arc::new(StandInModel(model_calls.clone(), "unused")))
        .with_capabilities(compute.clone())
        .with_executor(Arc::new(CountingExecutor(compute, execute_calls.clone())))
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(Arc::new(TestLocalReasoner::default().on(
            EvidenceState::Unknown,
            LocalReasoningResult::Continue {
                rationale: "the demo continues".into(),
            },
        )));
    if let Ok(found) = agent.discover_capabilities().await.result {
        for capability in found {
            if let CapabilityAvailability::Unavailable(reason)
            | CapabilityAvailability::Misconfigured(reason) = capability.availability
            {
                println!("SKIPPED — Compute unavailable ({reason})");
                return 3;
            }
        }
    }
    let limits = WorkLimits {
        max_turns: 6,
        max_executions: 2,
    };
    let spec = WorkSpec::new(
        WorkId::new("real-work"),
        WorkGoal::new("Run the Compute self test and determine whether it succeeded"),
    )
    .with_limits(limits);
    let report = agent
        .run_work(
            &spec,
            &ReactToObservation,
            &ModelDecisions(vec![], AtomicUsize::new(0)),
        )
        .await;
    let m = report.measurement();
    let violations = verify_trajectory(&report.events, &limits);
    let receipt = report
        .observations
        .first()
        .and_then(|o| o.receipt_id.clone());
    if json {
        // No receipt: it is a per-run identifier.
        println!("{}", measurement_json("real-compute", &m));
    } else {
        print_report(
            "Bounded autonomous work (real Compute)",
            &report,
            model_calls.load(Ordering::SeqCst),
            execute_calls.load(Ordering::SeqCst),
        );
        println!("\nMeasurement (real Compute):");
        println!("  compute_latency: {} ms", ms(m.compute_latency));
        println!("  executions: {}", m.executions);
        println!("  receipt: {}", receipt.as_deref().unwrap_or("none"));
        println!(
            "  observation: {}",
            report
                .observations
                .first()
                .map_or("none", |o| o.kind.as_str())
        );
        println!("  total_latency: {} ms", ms(m.total_latency));
    }
    if !violations.is_empty() {
        eprintln!("\nerror: trajectory invariants violated: {violations:?}");
        return 1;
    }
    if report.summary.terminal_state == TerminalState::Completed
        && report.summary.executions == 1
        && receipt.is_some()
    {
        if !json {
            println!(
                "\nExpected terminal state reached: completed (receipt {})",
                receipt.unwrap()
            );
        }
        0
    } else {
        eprintln!(
            "\nerror: the real workload did not complete: {:?}",
            report.outcome
        );
        1
    }
}

/// Escalates the very first decision to the model; afterwards reacts to what was observed.
struct AskModelFirst;

impl LocalWorkPolicy for AskModelFirst {
    fn propose(&self, view: &WorkView<'_>) -> Option<WorkDecision> {
        if view.turn == 0 {
            None
        } else {
            ReactToObservation.propose(view)
        }
    }
}

/// `--test-real-model-work [--json] [--deterministic-executor]`
///
/// A real FX provider (the `CHIP_*` configuration every live mode uses) makes the first decision;
/// Compute executes what it asks for; Chip observes the result and decides the next step itself.
/// Exit 3 means skipped: no provider configured, or no Compute. `--deterministic-executor` swaps
/// Compute for a fixed executor, for exercising the provider path where Compute is absent; it
/// is never the default.
pub async fn test_real_model_work(args: &[String]) -> i32 {
    let json = args.iter().any(|a| a == "--json");
    let deterministic_executor = args.iter().any(|a| a == "--deterministic-executor");

    let config = match crate::config_from_env(|name| std::env::var(name).ok()) {
        Ok(config) => config,
        Err(e) => {
            // The message names the missing variable, never a value.
            println!("SKIPPED: real model provider unavailable ({e})");
            return 3;
        }
    };
    let model_name = config.model.to_string();
    let provider = match fx_provider_http::HttpProvider::new(config) {
        Ok(provider) => provider,
        Err(e) => {
            println!("SKIPPED: real model provider unavailable ({e})");
            return 3;
        }
    };

    let execute_calls = Arc::new(AtomicUsize::new(0));
    let compute = Arc::new(chip_compute::ComputeExecutor::new());
    let builder = Agent::with_model(Arc::new(provider), model_name)
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(Arc::new(TestLocalReasoner::default()));
    let agent = if deterministic_executor {
        builder
            .with_capabilities(Arc::new(SelfTestCapability))
            .with_executor(Arc::new(CountingExecutor(
                Arc::new(Fixed(ExecutionStatus::Success)),
                execute_calls.clone(),
            )))
    } else {
        builder
            .with_capabilities(compute.clone())
            .with_executor(Arc::new(CountingExecutor(compute, execute_calls.clone())))
    };
    if let Ok(found) = agent.discover_capabilities().await.result {
        for capability in found {
            if let CapabilityAvailability::Unavailable(reason)
            | CapabilityAvailability::Misconfigured(reason) = capability.availability
            {
                println!("SKIPPED: Compute unavailable ({reason})");
                return 3;
            }
        }
    }

    let limits = WorkLimits {
        max_turns: 4,
        max_executions: 2,
    };
    let spec = WorkSpec::new(
        WorkId::new("real-model-work"),
        WorkGoal::new("Determine the next action for the compute.selftest capability"),
    )
    .with_limits(limits);
    let report = agent
        .run_work(&spec, &AskModelFirst, &ModelDecisionBoundary)
        .await;
    let m = report.measurement();
    let violations = verify_trajectory(&report.events, &limits);

    if json {
        println!("{}", measurement_json("real-model", &m));
    } else {
        let secs = |d: Duration| {
            if d >= Duration::from_secs(1) {
                format!("{:.2} s", d.as_secs_f64())
            } else {
                format!("{:.0} ms", d.as_secs_f64() * 1000.0)
            }
        };
        println!("Real model autonomous work");
        println!("--------------------------");
        let outcome = match &report.outcome {
            WorkOutcome::Completed { .. } => "Completed".to_string(),
            WorkOutcome::Escalated { reason } => format!("Escalated ({reason})"),
            WorkOutcome::Blocked { reason } => format!("Blocked ({reason})"),
            WorkOutcome::LimitReached { limit } => format!("LimitReached ({})", limit.name()),
            WorkOutcome::Failed { reason } => format!("Failed ({reason})"),
        };
        println!("Outcome:       {outcome}");
        println!("Turns:         {}", m.turns);
        println!("Executions:    {}", m.executions);
        println!("Observations:  {}", m.observations);
        println!("Local:         {}", m.local_decisions);
        println!("Escalations:   {}", m.model_escalations);
        println!("Model calls:   {}", m.model_calls);
        println!("Context:       {} bytes", m.context_bytes);
        println!(
            "Model tokens:  {}",
            m.model_tokens
                .map_or("not reported".to_string(), |t| t.to_string())
        );
        println!("Model latency: {}", secs(m.model_latency));
        println!("Compute:       {}", secs(m.compute_latency));
        println!("Total:         {}", secs(m.total_latency));
        println!("\nTrajectory:");
        for (i, event) in report.events.iter().enumerate() {
            println!("  {:>2}. {}", i + 1, describe(event));
        }
    }

    if !violations.is_empty() {
        eprintln!("\nerror: trajectory invariants violated: {violations:?}");
        return 1;
    }
    let as_expected = matches!(report.outcome, WorkOutcome::Completed { .. })
        && m.model_calls == 1
        && m.model_escalations == 1
        && m.executions == 1
        && m.observations == 1;
    if as_expected {
        0
    } else {
        eprintln!(
            "\nerror: the workload did not follow the expected trajectory (outcome: {:?}); \
             the model was asked to request compute.selftest",
            report.outcome
        );
        1
    }
}
