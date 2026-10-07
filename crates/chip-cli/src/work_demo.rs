//! `chip --test-work [scenario]` and `--test-real-work`: the bounded autonomous loop, end to end.
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
        WorkEvent::ContextLimit {
            request_bytes,
            budget_bytes,
            ..
        } => format!(
            "ContextLimit: request of {request_bytes} bytes exceeds the budget of {budget_bytes}; not sent"
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
        WorkEvent::GoalEvaluated {
            satisfied,
            remaining,
            ..
        } => format!(
            "GoalEvaluated: {} ({remaining} required output(s) remaining)",
            if *satisfied {
                "the observation satisfies the goal"
            } else {
                "the observation does not satisfy the goal"
            }
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

/// One block per escalation: which policy chose the context, and what was actually sent.
fn print_escalation_context(report: &WorkReport, policy: Option<&str>) {
    for context in &report.escalations {
        println!("\nEscalation context");
        println!("  policy: {}", policy.unwrap_or("unknown"));
        println!("  bytes: {}", context.bytes);
        println!("  chars: {}", context.chars);
        println!("  observations: {}", context.observations);
        println!("  decisions: {}", context.decisions);
        println!("  evidence: {}", context.evidence_items);
        println!("  ruled-out: {}", context.ruled_out);
    }
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
    print_escalation_context(report, m.context_policy.as_deref());
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

/// Keeps what the provider returned, so a successful reply can be saved as a regression
/// fixture (`--print-reply`). It records only the model's output: no prompt, no credentials.
struct RecordingProvider {
    inner: fx_provider_http::HttpProvider,
    replies: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl fx_core::ModelProvider for RecordingProvider {
    async fn complete(
        &self,
        request: fx_core::ModelRequest,
    ) -> Result<fx_core::ModelResponse, fx_core::FxError> {
        let response = self.inner.complete(request).await?;
        self.replies.lock().unwrap().push(response.output.clone());
        Ok(response)
    }
}

/// Escalates the very first decision to the model; afterwards reports what was observed. The
/// completion summary is the executor's own output and receipt, never anything the model said.
struct AskModelFirst;

impl LocalWorkPolicy for AskModelFirst {
    fn propose(&self, view: &WorkView<'_>) -> Option<WorkDecision> {
        if view.turn == 0 {
            return None;
        }
        Some(match view.observations.last() {
            Some(o) if o.kind == ObservationKind::ExecutionCompleted => WorkDecision::Complete {
                summary: format!(
                    "{} (receipt {})",
                    o.output.as_deref().unwrap_or("").trim(),
                    o.receipt_id.as_deref().unwrap_or("none")
                ),
            },
            Some(o) if o.kind == ObservationKind::ExecutionFailed => WorkDecision::Block {
                reason: "the requested capability failed".into(),
            },
            _ => WorkDecision::Escalate {
                reason: "the execution was cancelled".into(),
            },
        })
    }
}

/// What the offline (`--deterministic-executor`) path answers per capability. It stands in for
/// Compute only to exercise the provider path; its output is never evidence of anything.
struct DemoOutputs {
    hash_id: &'static str,
    system_info_id: &'static str,
}

#[async_trait::async_trait]
impl Executor for DemoOutputs {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        let output = if request.intent == self.hash_id {
            chip_compute::HASH_EXPECTED_SHA256
        } else if request.intent == self.system_info_id {
            "python demo on demo"
        } else {
            "self test passed"
        };
        Ok(ExecutionResult::success(request.id, output).with_receipt_id("sha256:demo-receipt"))
    }
}

/// The PR33 goal, which repeats the vocabulary of the digest capability's description.
const DEFAULT_GOAL: &str = "Determine the SHA-256 digest of the fixed test input.";

/// PR35: three equivalent goals for the same operation. None uses the words of the capability's
/// description ("SHA-256", "digest", "fixed test input"), so choosing needs meaning, not a match.
const SEMANTIC_GOALS: [&str; 3] = [
    "Produce the canonical fingerprint of the test payload.",
    "Calculate the deterministic identity value for the supplied test data.",
    "Return the standard cryptographic representation of the fixed payload.",
];

/// PR37: goals that describe the wanted outcome without any word the capabilities are described
/// with, and without the words that name the operation or its obvious synonyms. `goal_vocabulary
/// _violations` checks every one mechanically; a goal that fails it is refused at run time.
const ZERO_OVERLAP_GOALS: [&str; 6] = [
    "Obtain a short code summarising the stored sample text, such that altering even one character of that text would change the code.",
    "Establish a compact value for the reference passage that lets anyone later confirm the passage has not been tampered with.",
    "Condense the benchmark sample text into a constant-size value that cannot feasibly be reversed to recover the original wording.",
    "Derive an irreversible, constant-size summary of the canonical sample passage so two copies can be compared without exchanging the passage itself.",
    "Generate a tamper-evident marker for the reference material, allowing a recipient to verify nothing was modified in transit.",
    "Create a short unique token for the benchmark text, such that any single-character edit to the text yields a different token.",
];

/// Words that name the operation, its algorithm, its obvious synonyms or its implementation.
/// A goal containing one has told the model the answer in the capability's own language.
const NAMING_WORDS: &[&str] = &[
    "sha",
    "sha1",
    "sha256",
    "256",
    "hash",
    "hashing",
    "hashed",
    "digest",
    "checksum",
    "fingerprint",
    "cryptographic",
    "crypto",
    "md5",
    "crc",
    "hashlib",
    "python",
    "selftest",
    "operation",
    "capability",
];

const STOPWORDS: &[&str] = &[
    "a", "an", "the", "of", "to", "and", "or", "for", "in", "on", "at", "by", "with", "that",
    "this", "it", "its", "is", "are", "be", "can", "so", "such", "as", "from", "than", "then",
    "any", "even", "one", "two", "no", "not", "if", "when", "which", "who", "what", "into", "over",
    "per", "each", "has", "have", "had", "was", "were", "been", "do", "does", "will", "would",
    "should", "could", "may", "might", "must", "anyone", "itself", "there", "their", "they",
    "them", "these", "those", "we", "you", "your", "our", "us", "how", "why", "also", "only",
    "just", "both", "without", "within", "about", "after", "before", "later", "nothing",
];

fn vocabulary_tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty() && !STOPWORDS.contains(t))
        .map(str::to_string)
        .collect()
}

/// Two words count as the same vocabulary when they are equal, when one begins with the other
/// (three letters or more: "run" and "running"), or when they share their first five letters
/// ("determine" and "deterministic").
fn same_vocabulary(a: &str, b: &str) -> bool {
    a == b
        || (a.len() >= 3 && b.len() >= 3 && (a.starts_with(b) || b.starts_with(a)))
        || (a.len() >= 5 && b.len() >= 5 && a[..5] == b[..5])
}

/// Every reason `goal` is not a zero-overlap goal: a word it shares with any capability's
/// description or id, or a word that names the operation. Empty means the goal is clean.
fn goal_vocabulary_violations(goal: &str) -> Vec<String> {
    let described = [
        chip_compute::HASH_DESCRIPTION,
        chip_compute::SYSTEM_INFO_DESCRIPTION,
        chip_compute::SELFTEST_DESCRIPTION,
        chip_compute::OP_A_INTENT,
        chip_compute::OP_B_INTENT,
        chip_compute::OP_C_INTENT,
        chip_compute::HASH_INTENT,
        chip_compute::SYSTEM_INFO_INTENT,
        chip_compute::SELFTEST_INTENT,
    ];
    let capability_words: Vec<String> = described
        .iter()
        .flat_map(|d| vocabulary_tokens(d))
        .collect();
    let mut found = Vec::new();
    for word in vocabulary_tokens(goal) {
        if NAMING_WORDS.iter().any(|n| {
            // Short names ("sha", "256", "md5", "crc") match exactly; longer ones by stem.
            if n.len() < 4 {
                word == *n
            } else {
                same_vocabulary(&word, n)
            }
        }) {
            found.push(format!("'{word}' names the operation"));
        } else if let Some(c) = capability_words.iter().find(|c| same_vocabulary(&word, c)) {
            found.push(format!(
                "'{word}' shares vocabulary with a capability ('{c}')"
            ));
        }
    }
    found
}

const OPAQUE_IDS: [&str; 3] = [
    chip_compute::OP_A_INTENT,
    chip_compute::OP_B_INTENT,
    chip_compute::OP_C_INTENT,
];

/// PR38 fixture: the first model reply is read as usual and then replaced by a fixed request for a
/// capability known to be wrong. The request is valid in every way Chip checks (declared,
/// available, no invented inputs), so it goes through validation, real Compute execution,
/// observation and evidence exactly as a model's own choice would. Every later reply is read
/// normally. Only the *first judgment* is forced; nothing after it is.
struct ForcedFirstDecision {
    forced: std::sync::Mutex<Option<WorkDecision>>,
}

impl WorkDecisionBoundary for ForcedFirstDecision {
    fn interpret(
        &self,
        response: &ModelResponse,
        capabilities: &[Capability],
    ) -> Result<WorkDecision, DecisionError> {
        if let Some(decision) = self.forced.lock().unwrap().take() {
            return Ok(decision);
        }
        ModelDecisionBoundary.interpret(response, capabilities)
    }

    fn question(&self, capabilities: &[Capability]) -> String {
        ModelDecisionBoundary.question(capabilities)
    }
}

/// How a run that began with a forced wrong decision ended, read from the trajectory: did Chip
/// recover through authoritative evidence, and did it ever complete without the goal being met.
///
/// Returns the label and whether the work completed *without* any observation having satisfied
/// the goal (an unauthorized completion, which must never happen).
fn recovery_profile(
    report: &WorkReport,
    forced: Option<&str>,
    hash_id: &str,
    required: bool,
) -> (&'static str, bool) {
    let satisfied = report.events.iter().any(|e| {
        matches!(
            e,
            WorkEvent::GoalEvaluated {
                satisfied: true,
                ..
            }
        )
    });
    let completed = matches!(report.outcome, WorkOutcome::Completed { .. });
    // Only meaningful when the work had a requirement: without one nothing is ever evaluated.
    let false_completion = required && completed && !satisfied;
    let requested: Vec<String> = report
        .events
        .iter()
        .filter_map(|e| match e {
            WorkEvent::CapabilityRequested { capability, .. } => Some(capability.to_string()),
            _ => None,
        })
        .collect();
    let label = match forced {
        None => "n/a (no wrong decision was forced)",
        Some(wrong) => {
            let wrong_first = requested.first().map(String::as_str) == Some(wrong);
            let right_later = requested.iter().skip(1).any(|c| c == hash_id);
            match &report.outcome {
                WorkOutcome::Completed { .. } if satisfied && wrong_first && right_later => {
                    "recovered"
                }
                WorkOutcome::Completed { .. } => "UNAUTHORIZED COMPLETION",
                WorkOutcome::Blocked { reason } if reason.starts_with("completion refused") => {
                    "not recovered (the model claimed completion; Chip refused it)"
                }
                WorkOutcome::Blocked { .. } | WorkOutcome::Failed { .. } => {
                    "not recovered (the second decision was rejected)"
                }
                _ if report.summary.executions >= 2 && !satisfied => {
                    "not recovered (wrong again; stopped by a limit)"
                }
                _ => "not recovered (other)",
            }
        }
    };
    (label, false_completion)
}

/// splitmix64: the experiment's only source of "randomness", so every dealing is reproducible.
pub(crate) fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

pub(crate) fn shuffle<T>(items: &mut [T], state: &mut u64) {
    for i in (1..items.len()).rev() {
        items.swap(i, (next(state) % (i as u64 + 1)) as usize);
    }
}

/// How the three opaque ids are dealt for one run: which id carries each operation, and the order
/// the capabilities are presented in. It is experiment configuration, derived from a seed so a run
/// can be repeated exactly; it is not part of the runtime.
struct Dealing {
    hash: &'static str,
    system_info: &'static str,
    selftest: &'static str,
    order: [&'static str; 3],
    label: String,
}

impl Dealing {
    /// PR32: op_a digests, op_b reports the runtime, op_c is the self test; declared in id order.
    fn default_deal() -> Self {
        Self {
            hash: OPAQUE_IDS[0],
            system_info: OPAQUE_IDS[1],
            selftest: OPAQUE_IDS[2],
            order: OPAQUE_IDS,
            label: "default (op_a digests; op_a, op_b, op_c)".into(),
        }
    }

    /// Run `run` (1-based) of a seed. The six possible assignments are shuffled into a deck, so
    /// runs 1-6 use every assignment exactly once; the presentation order is shuffled
    /// independently for every run.
    fn from_seed(seed: u64, run: u64) -> Self {
        let mut deck = vec![
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        shuffle(&mut deck, &mut seed.clone());
        let [h, s, t] = deck[((run.max(1) - 1) % 6) as usize];
        let mut order = [0usize, 1, 2];
        shuffle(
            &mut order,
            &mut seed.wrapping_mul(1_000_003).wrapping_add(run),
        );
        Self {
            hash: OPAQUE_IDS[h],
            system_info: OPAQUE_IDS[s],
            selftest: OPAQUE_IDS[t],
            order: order.map(|i| OPAQUE_IDS[i]),
            label: format!("seed {seed} run {run}"),
        }
    }
}

/// How one run went at the capability boundary, read from the trajectory (never from the model's
/// reply): was a declared capability selected, was it invoked, and did the goal come out met.
///
/// * A: the right capability was selected and invoked, and the observation met the goal
/// * B: the right capability was selected, but the invocation was rejected (invented inputs);
///   nothing executed
/// * C: a wrong but declared capability was selected and invoked; it ran and the goal stayed unmet
/// * D: an undeclared (or unavailable) capability was named; rejected before any selection
/// * E: the reply was not a usable decision at all (prose, malformed, a forbidden field...)
/// * F: a wrong capability was selected and its invocation was rejected as well
/// * G: the reply was a valid decision that selects no capability (complete, escalate or block)
/// * `-`: no decision was reached (the model call itself failed) or the run ended otherwise
fn capability_profile(
    report: &WorkReport,
    hash_id: &str,
    goal_met: bool,
) -> (String, String, &'static str) {
    let model_answered = report.events.iter().any(|e| {
        matches!(
            e,
            WorkEvent::ModelCalled {
                succeeded: true,
                ..
            }
        )
    });
    let selected = report.events.iter().find_map(|e| match e {
        WorkEvent::CapabilityRequested { capability, .. } => Some(capability.to_string()),
        _ => None,
    });
    let executed = report.summary.executions > 0;
    let invocation_rejected = matches!(&report.outcome, WorkOutcome::Blocked { reason }
        if reason.starts_with("invalid capability input"));
    match (model_answered, selected) {
        (false, _) => (
            "none (the model call failed)".into(),
            "not reached".into(),
            "-",
        ),
        (true, None) if !matches!(&report.outcome, WorkOutcome::Failed { .. }) => (
            "none (a valid decision that selects no capability)".into(),
            "not reached".into(),
            "G",
        ),
        (true, None) => {
            let named_unusable = matches!(&report.outcome, WorkOutcome::Failed { reason }
                if reason.contains("unknown capability") || reason.contains("capabilities unavailable"));
            if named_unusable {
                (
                    "rejected (an undeclared or unavailable capability)".into(),
                    "not reached".into(),
                    "D",
                )
            } else {
                (
                    "rejected (the reply was not a usable decision)".into(),
                    "not reached".into(),
                    "E",
                )
            }
        }
        (true, Some(capability)) => {
            let right = capability == hash_id;
            let selection = format!("valid ({capability})");
            if invocation_rejected && !executed {
                (
                    selection,
                    "rejected (inputs the capability does not declare; nothing executed)".into(),
                    if right { "B" } else { "F" },
                )
            } else if executed {
                let category = match (right, goal_met) {
                    (true, true) => "A",
                    (false, _) => "C",
                    (true, false) => "-",
                };
                (selection, "valid (executed by Compute)".into(), category)
            } else {
                (selection, "not completed".into(), "-")
            }
        }
    }
}

impl Dealing {
    /// PR37: run `run` (1-based) of a *balanced* dealing. A seeded search finds an assignment deck
    /// and an order deck, each of the six possible permutations, such that across the six runs
    /// every operation appears in every presented position exactly twice, and every id carries the
    /// digest exactly twice. So no fixed position and no fixed id (and no first-in-identifier-
    /// order choice) can score above one in three. The dealing does not depend on the goal.
    fn balanced(seed: u64, run: u64) -> Self {
        let perms: [[usize; 3]; 6] = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        let mut attempt = 0u64;
        let (assignments, orders) = loop {
            let mut state = seed ^ attempt.wrapping_mul(0xD6E8_FEB8_6659_FD93);
            let mut assignments = perms;
            let mut orders = perms;
            shuffle(&mut assignments, &mut state);
            shuffle(&mut orders, &mut state);
            // Operation `role` (0 digest, 1 runtime info, 2 self test) sits at position
            // `position` in run k when its id is at that index of the presented order.
            let balanced = (0..3).all(|role| {
                let mut at = [0usize; 3];
                for (a, o) in assignments.iter().zip(orders.iter()) {
                    at[o.iter().position(|id| *id == a[role]).unwrap()] += 1;
                }
                at == [2, 2, 2]
            });
            if balanced {
                break (assignments, orders);
            }
            attempt += 1;
            assert!(
                attempt < 1_000_000,
                "no balanced dealing found for seed {seed}"
            );
        };
        let k = ((run.max(1) - 1) % 6) as usize;
        let [h, s, t] = assignments[k];
        Self {
            hash: OPAQUE_IDS[h],
            system_info: OPAQUE_IDS[s],
            selftest: OPAQUE_IDS[t],
            order: orders[k].map(|i| OPAQUE_IDS[i]),
            label: format!("seed {seed} run {run} (balanced)"),
        }
    }
}

/// Presents another provider's capabilities in a chosen order. The set and the availability are
/// the inner provider's; only the order the model reads them in differs.
struct Reordered(Arc<dyn CapabilityProvider>, [&'static str; 3]);

#[async_trait::async_trait]
impl CapabilityProvider for Reordered {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        let mut found = self.0.capabilities().await?;
        found.sort_by_key(|d| {
            self.1
                .iter()
                .position(|id| *id == d.id.as_str())
                .unwrap_or(usize::MAX)
        });
        Ok(found)
    }

    async fn availability(&self, id: &CapabilityId) -> CapabilityAvailability {
        self.0.availability(id).await
    }
}

/// A fixed, always-available set of capability descriptors (offline path only).
struct DeclaredCapabilities(Vec<CapabilityDescriptor>);

#[async_trait::async_trait]
impl CapabilityProvider for DeclaredCapabilities {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        Ok(self.0.clone())
    }

    async fn availability(&self, _id: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
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
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse::<u64>().ok())
    };
    // PR38: `--require-output` makes the known digest the goal's requirement, so completing needs
    // an authoritative observation that equals it; `--force-wrong-first [--wrong-role ROLE]` also
    // replaces the first decision with a valid request for the wrong capability.
    let force_wrong_first = args.iter().any(|a| a == "--force-wrong-first");
    let require_output = force_wrong_first || args.iter().any(|a| a == "--require-output");
    let wrong_role = args
        .iter()
        .position(|a| a == "--wrong-role")
        .and_then(|i| args.get(i + 1))
        .map(String::as_str);
    if require_output && deterministic_executor {
        eprintln!(
            "error: the recovery experiment needs real Compute; --deterministic-executor is not allowed with --require-output or --force-wrong-first"
        );
        return 2;
    }
    if !force_wrong_first && wrong_role.is_some() {
        eprintln!("error: --wrong-role needs --force-wrong-first");
        return 2;
    }
    if force_wrong_first && !matches!(wrong_role, None | Some("runtime-info" | "self-test")) {
        eprintln!("error: --wrong-role is runtime-info or self-test");
        return 2;
    }
    // `--permutation-seed S --permutation-run N`: deal the opaque ids from a seed (PR33).
    let balanced = args.iter().any(|a| a == "--balanced-dealing");
    let dealing = match (flag("--permutation-seed"), flag("--permutation-run")) {
        (Some(seed), Some(run)) if balanced => Dealing::balanced(seed, run),
        (Some(seed), Some(run)) => Dealing::from_seed(seed, run),
        (None, None) if balanced => {
            eprintln!("error: --balanced-dealing needs --permutation-seed and --permutation-run");
            return 2;
        }
        (None, None) => Dealing::default_deal(),
        _ => {
            eprintln!("error: --permutation-seed and --permutation-run go together (both numbers)");
            return 2;
        }
    };

    // `--zero-overlap-goal N` (1-6): one of the PR37 goals. A goal that fails the vocabulary guard
    // is refused before any model is asked.
    if args.iter().any(|a| a == "--zero-overlap-goal")
        && args.iter().any(|a| a == "--semantic-goal")
    {
        eprintln!("error: --zero-overlap-goal and --semantic-goal are alternatives");
        return 2;
    }
    let zero_overlap = match (
        args.iter().any(|a| a == "--zero-overlap-goal"),
        flag("--zero-overlap-goal"),
    ) {
        (false, _) => None,
        (true, Some(n @ 1..=6)) => {
            let goal = ZERO_OVERLAP_GOALS[n as usize - 1];
            let violations = goal_vocabulary_violations(goal);
            if !violations.is_empty() {
                eprintln!(
                    "error: goal Z{n} is not zero-overlap: {}",
                    violations.join("; ")
                );
                return 2;
            }
            Some((goal, format!("Z{n}")))
        }
        (true, _) => {
            eprintln!("error: --zero-overlap-goal takes a number from 1 to 6");
            return 2;
        }
    };

    // `--semantic-goal N` (1-3): one of the PR35 goals instead of the PR33 one.
    let (goal, goal_label) = if let Some(chosen) = zero_overlap {
        chosen
    } else {
        match flag("--semantic-goal") {
            None if args.iter().any(|a| a == "--semantic-goal") => {
                eprintln!("error: --semantic-goal takes a number from 1 to 3");
                return 2;
            }
            None => (DEFAULT_GOAL, "default (PR33)".to_string()),
            Some(n @ 1..=3) => (
                SEMANTIC_GOALS[n as usize - 1],
                format!("{}", (b'A' + n as u8 - 1) as char),
            ),
            Some(_) => {
                eprintln!("error: --semantic-goal takes a number from 1 to 3");
                return 2;
            }
        }
    };

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
    let replies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = RecordingProvider {
        inner: provider,
        replies: replies.clone(),
    };

    let execute_calls = Arc::new(AtomicUsize::new(0));
    let compute = Arc::new(chip_compute::ComputeExecutor::new().with_opaque_assignment(
        dealing.hash,
        dealing.system_info,
        dealing.selftest,
    ));
    let builder = Agent::with_model(Arc::new(provider), model_name)
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(Arc::new(TestLocalReasoner::default()));
    let agent = if deterministic_executor {
        // Descriptors only: nothing runs to describe them.
        let descriptors = compute.capabilities().await.unwrap_or_default();
        builder
            .with_capabilities(Arc::new(Reordered(
                Arc::new(DeclaredCapabilities(descriptors)),
                dealing.order,
            )))
            .with_executor(Arc::new(CountingExecutor(
                Arc::new(DemoOutputs {
                    hash_id: dealing.hash,
                    system_info_id: dealing.system_info,
                }),
                execute_calls.clone(),
            )))
    } else {
        builder
            .with_capabilities(Arc::new(Reordered(compute.clone(), dealing.order)))
            .with_executor(Arc::new(CountingExecutor(compute, execute_calls.clone())))
    };
    let mut offered = Vec::new();
    if let Ok(found) = agent.discover_capabilities().await.result {
        for capability in found {
            offered.push(capability.descriptor.id.to_string());
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
    let mut spec =
        WorkSpec::new(WorkId::new("real-model-work"), WorkGoal::new(goal)).with_limits(limits);
    if require_output {
        spec = spec.with_required_output(chip_compute::HASH_EXPECTED_SHA256);
    }
    let forced_id: Option<&'static str> = force_wrong_first.then(|| match wrong_role {
        Some("self-test") => dealing.selftest,
        _ => dealing.system_info,
    });
    let forcing = ForcedFirstDecision {
        forced: std::sync::Mutex::new(forced_id.map(|id| {
            WorkDecision::RequestCapability(CapabilityRequest::new(
                ExecutionId::new("forced-wrong-first"),
                CapabilityId::new(id).expect("an opaque id is a valid capability id"),
            ))
        })),
    };
    let report = agent.run_work(&spec, &AskModelFirst, &forcing).await;
    let m = report.measurement();
    let violations = verify_trajectory(&report.events, &limits);
    let receipt = report
        .observations
        .last()
        .and_then(|o| o.receipt_id.clone());

    // What the model chose, and what reality answered. The expected digest is known
    // independently of the model and of Compute.
    let requested: Vec<String> = report
        .events
        .iter()
        .filter_map(|e| match e {
            WorkEvent::CapabilityRequested { capability, .. } => Some(capability.to_string()),
            _ => None,
        })
        .collect();
    // With a forced first decision the capability that matters for selection is the recovery one.
    let chosen = if force_wrong_first {
        requested.last().cloned().filter(|_| requested.len() > 1)
    } else {
        requested.first().cloned()
    };
    let observed = report
        .observations
        .last()
        .and_then(|o| o.output.as_deref())
        .map(|o| o.trim().to_string());
    let digest_ok = observed.as_deref() == Some(chip_compute::HASH_EXPECTED_SHA256)
        && matches!(&report.outcome, WorkOutcome::Completed { summary }
            if summary.contains(chip_compute::HASH_EXPECTED_SHA256));

    let (selection, invocation, category) = capability_profile(&report, dealing.hash, digest_ok);
    let (recovery, false_completion) =
        recovery_profile(&report, forced_id, dealing.hash, require_output);

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
        println!("Outcome:           {outcome}");
        println!("Turns:             {}", m.turns);
        println!("Executions:        {}", m.executions);
        println!("Observations:      {}", m.observations);
        println!("Local decisions:   {}", m.local_decisions);
        println!("Model escalations: {}", m.model_escalations);
        println!("Model calls:       {}", m.model_calls);
        println!("Context bytes:     {}", m.context_bytes);
        println!(
            "Model tokens:      {}",
            m.model_tokens
                .map_or("not reported".to_string(), |t| t.to_string())
        );
        println!("Model latency:     {}", secs(m.model_latency));
        println!("Compute latency:   {}", secs(m.compute_latency));
        println!("Total latency:     {}", secs(m.total_latency));
        println!(
            "Receipt:           {}",
            receipt.as_deref().unwrap_or("none")
        );
        print_escalation_context(&report, m.context_policy.as_deref());
        println!("\nCapability selection");
        println!("  goal variant:    {goal_label}");
        println!("  permutation:     {}", dealing.label);
        println!("  offered:         {}", offered.join(", "));
        println!("  hash capability: {}", dealing.hash);
        println!(
            "  assignment:      digest={} runtime-info={} self-test={}",
            dealing.hash, dealing.system_info, dealing.selftest
        );
        println!(
            "  evidence recorded: {}",
            report
                .events
                .iter()
                .filter(|e| matches!(e, WorkEvent::EvidenceRecorded { .. }))
                .count()
        );
        println!("  chosen:          {}", chosen.as_deref().unwrap_or("none"));
        println!("  selection:       {selection}");
        println!("  invocation:      {invocation}");
        println!("  category:        {category}");
        println!(
            "  observed output: {}",
            observed.as_deref().unwrap_or("none")
        );
        println!("  expected digest: {}", chip_compute::HASH_EXPECTED_SHA256);
        println!(
            "  digest from the Compute observation matches: {}",
            if digest_ok { "yes" } else { "no" }
        );
        if require_output {
            let evaluations: Vec<&str> = report
                .events
                .iter()
                .filter_map(|e| match e {
                    WorkEvent::GoalEvaluated { satisfied, .. } => Some(if *satisfied {
                        "satisfied"
                    } else {
                        "not satisfied"
                    }),
                    _ => None,
                })
                .collect();
            println!("\nRecovery");
            println!("  requested:       {}", requested.join(", "));
            println!(
                "  first decision:  {}",
                forced_id.map_or("the model's own".to_string(), |id| format!(
                    "forced wrong ({id}); the model's first reply was overridden by the harness"
                ))
            );
            println!("  goal evaluations: {}", evaluations.join(", "));
            println!("  recovery:        {recovery}");
            println!(
                "  unauthorized completion: {}",
                if false_completion { "YES" } else { "no" }
            );
        }
        println!("\nTrajectory:");
        for (i, event) in report.events.iter().enumerate() {
            println!("  {:>2}. {}", i + 1, describe(event));
        }
    }

    if !violations.is_empty() {
        eprintln!("\nerror: trajectory invariants violated: {violations:?}");
        return 1;
    }
    if args.iter().any(|a| a == "--print-reply") {
        // The model's own output, for saving as a sanitized fixture.
        for reply in replies.lock().unwrap().iter() {
            println!("\nModel reply:\n{reply}");
        }
    }
    // Forced wrong first: a wrong execution, a recovery escalation, a right execution, then the
    // local completion. Otherwise the single-decision shape of every earlier experiment.
    let (turns, calls, execs) = if force_wrong_first {
        (3, 2, 2)
    } else {
        (2, 1, 1)
    };
    let as_expected = matches!(report.outcome, WorkOutcome::Completed { .. })
        && !false_completion
        && m.turns as usize == turns
        && m.local_decisions == 1
        && (deterministic_executor || receipt.is_some())
        && m.model_calls as usize == calls
        && m.model_escalations as usize == calls
        && m.executions as usize == execs
        && m.observations as usize == execs
        && chosen.as_deref() == Some(dealing.hash)
        && digest_ok;
    if as_expected {
        0
    } else {
        eprintln!(
            "\nerror: the workload did not follow the expected trajectory (outcome: {:?}); \
             the model was asked to choose a capability that reports the SHA-256 digest of a \
             known text (expected the capability described as producing the SHA-256 digest, answered by the Compute observation)",
            report.outcome
        );
        1
    }
}

#[cfg(test)]
mod pr37 {
    //! PR37: the zero-overlap corpus, its vocabulary guard, and the balanced dealing.

    use super::*;

    /// Goals from earlier experiments. Each leaks vocabulary, which is why PR37 replaced them.
    const LEAKY_GOALS: [&str; 5] = [
        DEFAULT_GOAL,
        "Produce the canonical fingerprint of the test payload.",
        "Calculate the deterministic identity value for the supplied test data.",
        "Return the standard cryptographic representation of the fixed payload.",
        "Determine the SHA-256 digest of the fixed test input.",
    ];

    #[test]
    fn the_corpus_is_large_distinct_and_clean() {
        assert!(ZERO_OVERLAP_GOALS.len() >= 6);
        for (i, goal) in ZERO_OVERLAP_GOALS.iter().enumerate() {
            let violations = goal_vocabulary_violations(goal);
            assert!(violations.is_empty(), "Z{}: {goal}: {violations:?}", i + 1);
            assert!(
                goal.ends_with('.') && goal.split_whitespace().count() >= 10,
                "Z{}",
                i + 1
            );
            for other in &ZERO_OVERLAP_GOALS[i + 1..] {
                assert_ne!(goal, other, "duplicate goal");
            }
        }
    }

    #[test]
    fn the_guard_rejects_every_leaky_goal_from_earlier_experiments() {
        // The guard is what separates PR37 from PR35: it must fail on all of PR33/PR35's wording.
        for goal in LEAKY_GOALS {
            assert!(
                !goal_vocabulary_violations(goal).is_empty(),
                "the guard let through: {goal}"
            );
        }
    }

    #[test]
    fn the_guard_names_what_it_found() {
        let found =
            goal_vocabulary_violations("Determine the SHA-256 digest of the fixed test input.");
        let text = found.join(" | ");
        for word in ["sha", "digest", "fixed", "test", "input", "256"] {
            assert!(
                text.contains(&format!("'{word}'")),
                "{word} not reported: {text}"
            );
        }
    }

    #[test]
    fn the_guard_catches_stems_synonyms_and_every_capabilitys_words() {
        for (goal, needle) in [
            ("Compute the hashing of the sample.", "hashing"),
            ("Give me a checksum of the sample.", "checksum"),
            ("Give me a fingerprint of the sample.", "fingerprint"),
            (
                "Apply a cryptographic summary to the sample.",
                "cryptographic",
            ),
            ("Determine a summary of the sample.", "determine"), // stem of "deterministic"
            ("Running a summary of the sample.", "running"),     // begins with "run"
            ("Report on the sample.", "report"),
            ("Tell me the runtime of the sample.", "runtime"),
            ("Show the information about the sample.", "information"),
            ("Summarise the existing sample.", "existing"),
            ("Use the self-test on the sample.", "self"),
            ("Summarise the inputs.", "inputs"),
            ("Summarise the tested sample.", "tested"),
            ("Use compute.op_a on the sample.", "compute"),
        ] {
            let found = goal_vocabulary_violations(goal);
            assert!(
                found.iter().any(|f| f.contains(&format!("'{needle}'"))),
                "{goal}: expected '{needle}' in {found:?}"
            );
        }
    }

    #[test]
    fn a_clean_goal_is_not_rejected_so_the_guard_is_not_a_blanket_no() {
        assert!(
            goal_vocabulary_violations("Obtain a short code for the stored sample text.")
                .is_empty()
        );
    }

    #[test]
    fn reintroducing_any_shortcut_word_into_any_goal_is_caught() {
        // A mutation check on the corpus itself: insert each naming word, and one word from each
        // description, into each goal; the guard must object every time.
        let mut words: Vec<&str> = NAMING_WORDS.to_vec();
        words.extend([
            "fixed",
            "report",
            "deterministic",
            "runtime",
            "existing",
            "result",
        ]);
        for goal in ZERO_OVERLAP_GOALS {
            for word in &words {
                let mutated = format!("{goal} {word}");
                assert!(
                    !goal_vocabulary_violations(&mutated).is_empty(),
                    "'{word}' slipped into: {goal}"
                );
            }
        }
    }

    fn roles(d: &Dealing) -> [&'static str; 3] {
        [d.hash, d.system_info, d.selftest]
    }

    fn position_of(d: &Dealing, id: &str) -> usize {
        d.order.iter().position(|o| *o == id).unwrap()
    }

    #[test]
    fn the_balanced_dealing_covers_every_assignment_order_and_position() {
        for seed in [3201, 0, 1, 7, 42, 1_000_003, u64::MAX] {
            let runs: Vec<Dealing> = (1..=6).map(|r| Dealing::balanced(seed, r)).collect();

            // All six assignments of operations to ids, each once.
            let mut assignments: Vec<[&str; 3]> = runs.iter().map(roles).collect();
            assignments.sort();
            assignments.dedup();
            assert_eq!(assignments.len(), 6, "seed {seed}: assignments");

            // All six presentation orders, each once; some are not in id order.
            let mut orders: Vec<[&str; 3]> = runs.iter().map(|d| d.order).collect();
            orders.sort();
            orders.dedup();
            assert_eq!(orders.len(), 6, "seed {seed}: orders");
            assert!(runs.iter().any(|d| d.order != OPAQUE_IDS), "seed {seed}");

            // Every operation sits in every position exactly twice.
            for role in 0..3 {
                let mut at = [0; 3];
                for d in &runs {
                    at[position_of(d, roles(d)[role])] += 1;
                }
                assert_eq!(at, [2, 2, 2], "seed {seed}: role {role} by position");
            }
            // Every id carries the digest exactly twice.
            for id in OPAQUE_IDS {
                assert_eq!(
                    runs.iter().filter(|d| d.hash == id).count(),
                    2,
                    "seed {seed}: {id}"
                );
            }
        }
    }

    #[test]
    fn every_shortcut_scores_exactly_one_third_on_a_balanced_dealing() {
        let runs: Vec<Dealing> = (1..=6).map(|r| Dealing::balanced(3201, r)).collect();
        let hits = |pick: &dyn Fn(&Dealing) -> &'static str| {
            runs.iter().filter(|d| pick(d) == d.hash).count()
        };
        assert_eq!(hits(&|d| d.order[0]), 2, "always first presented");
        assert_eq!(hits(&|d| d.order[1]), 2, "always second presented");
        assert_eq!(hits(&|d| d.order[2]), 2, "always last presented");
        for id in OPAQUE_IDS {
            assert_eq!(hits(&|_| id), 2, "always {id}");
        }
        let mut sorted_first = 0;
        for d in &runs {
            let mut ids = d.order;
            ids.sort();
            sorted_first += usize::from(ids[0] == d.hash);
        }
        assert_eq!(sorted_first, 2, "first in identifier order");
    }

    #[test]
    fn the_dealing_is_deterministic_and_the_seed_matters() {
        for run in 1..=6 {
            let a = Dealing::balanced(3201, run);
            let b = Dealing::balanced(3201, run);
            assert_eq!(
                (roles(&a), a.order, a.label.clone()),
                (roles(&b), b.order, b.label.clone())
            );
        }
        let signature = |seed| -> Vec<([&'static str; 3], [&'static str; 3])> {
            (1..=6)
                .map(|r| {
                    let d = Dealing::balanced(seed, r);
                    (roles(&d), d.order)
                })
                .collect()
        };
        assert_ne!(signature(3201), signature(3202));
        // Past six runs the deck repeats, it does not change.
        assert_eq!(
            roles(&Dealing::balanced(3201, 7)),
            roles(&Dealing::balanced(3201, 1))
        );
    }

    #[test]
    fn ids_stay_opaque_in_every_dealing() {
        for run in 1..=6 {
            let d = Dealing::balanced(3201, run);
            let mut ids = d.order.to_vec();
            ids.sort();
            assert_eq!(ids, OPAQUE_IDS);
            for id in d.order {
                for word in ["hash", "digest", "sha", "self", "test", "info", "system"] {
                    assert!(!id.contains(word), "{id} reveals {word}");
                }
            }
        }
    }

    #[test]
    fn the_earlier_dealing_is_unchanged() {
        // PR33-PR36 results depend on it; PR37 adds a mode and leaves this one alone.
        let d = Dealing::from_seed(3201, 1);
        assert_eq!(d.order, ["compute.op_c", "compute.op_a", "compute.op_b"]);
        assert_eq!(d.hash, "compute.op_b");
        let d = Dealing::from_seed(3201, 6);
        assert_eq!(d.order, ["compute.op_b", "compute.op_c", "compute.op_a"]);
        assert_eq!(d.hash, "compute.op_b");
    }
}
