//! PR27: the canonical measurement of an autonomous workload. Every count is derived from the event
//! trajectory; these tests recompute them independently and check that nothing disagrees.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chip_core::{
    Agent, Capability, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityInput, CapabilityProvider, CapabilityRequest, DecisionError, DecisionSource,
    EvidenceLookup, EvidenceState, ExecutionError, ExecutionEvent, ExecutionId, ExecutionObserver,
    ExecutionRequest, ExecutionResult, ExecutionStatus, Executor, InputValue, LimitKind,
    LocalReasoningResult, LocalWorkPolicy, ObservationKind, ScriptedPolicy, TestLocalReasoner,
    WorkDecision, WorkDecisionBoundary, WorkEvent, WorkGoal, WorkId, WorkLimits, WorkMeasurement,
    WorkOutcome, WorkReport, WorkSpec, WorkView, verify_trajectory,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

struct Fx {
    calls: AtomicUsize,
    seen: Mutex<Vec<ModelRequest>>,
    delay: Duration,
    fail: bool,
}

impl Fx {
    fn new() -> Arc<Fx> {
        Arc::new(Fx {
            calls: AtomicUsize::new(0),
            seen: Mutex::new(vec![]),
            delay: Duration::ZERO,
            fail: false,
        })
    }
    fn slow(delay: Duration) -> Arc<Fx> {
        Arc::new(Fx {
            calls: AtomicUsize::new(0),
            seen: Mutex::new(vec![]),
            delay,
            fail: false,
        })
    }
    fn failing() -> Arc<Fx> {
        Arc::new(Fx {
            calls: AtomicUsize::new(0),
            seen: Mutex::new(vec![]),
            delay: Duration::ZERO,
            fail: true,
        })
    }
}

#[async_trait::async_trait]
impl ModelProvider for Fx {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, FxError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(request);
        tokio::time::sleep(self.delay).await;
        if self.fail {
            return Err(FxError::Provider("down".into()));
        }
        Ok(ModelResponse::new("r", "reply", Usage::new(7, 3)))
    }
}

struct Exec {
    calls: AtomicUsize,
    status: ExecutionStatus,
    delay: Duration,
}

impl Exec {
    fn new(status: ExecutionStatus) -> Arc<Exec> {
        Arc::new(Exec {
            calls: AtomicUsize::new(0),
            status,
            delay: Duration::ZERO,
        })
    }
    fn slow(delay: Duration) -> Arc<Exec> {
        Arc::new(Exec {
            calls: AtomicUsize::new(0),
            status: ExecutionStatus::Success,
            delay,
        })
    }
}

#[async_trait::async_trait]
impl Executor for Exec {
    async fn execute(&self, r: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        tokio::time::sleep(self.delay).await;
        Ok(ExecutionResult {
            id: r.id,
            status: self.status,
            output: "out".into(),
            receipt_id: Some(format!("sha256:r{n}")),
        })
    }
}

struct Caps;

#[async_trait::async_trait]
impl CapabilityProvider for Caps {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        let mut d = CapabilityDescriptor::new(CapabilityId::new("compute.selftest")?, "t", "t");
        d.inputs.push(CapabilityInput {
            name: "n".into(),
            description: "".into(),
            required: false,
        });
        Ok(vec![d])
    }
    async fn availability(&self, _: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
    }
}

fn req(exec: &str) -> CapabilityRequest {
    CapabilityRequest::new(
        ExecutionId::new(exec),
        CapabilityId::new("compute.selftest").unwrap(),
    )
}

fn run(exec: &str) -> WorkDecision {
    WorkDecision::RequestCapability(req(exec))
}

fn done() -> WorkDecision {
    WorkDecision::Complete {
        summary: "done".into(),
    }
}

struct Script(Vec<WorkDecision>, AtomicUsize);

impl WorkDecisionBoundary for Script {
    fn interpret(
        &self,
        _: &ModelResponse,
        _: &[Capability],
    ) -> Result<WorkDecision, DecisionError> {
        let i = self.1.fetch_add(1, Ordering::SeqCst);
        self.0
            .get(i)
            .cloned()
            .ok_or_else(|| DecisionError::InvalidDecision("exhausted".into()))
    }
}

fn script(v: Vec<WorkDecision>) -> Script {
    Script(v, AtomicUsize::new(0))
}

fn reasoner() -> Arc<TestLocalReasoner> {
    Arc::new(
        TestLocalReasoner::default()
            .on(
                EvidenceState::Unknown,
                LocalReasoningResult::Continue {
                    rationale: "ok".into(),
                },
            )
            .on(
                EvidenceState::KnownStale,
                LocalReasoningResult::Continue {
                    rationale: "ok".into(),
                },
            ),
    )
}

fn agent(fx: &Arc<Fx>, exec: &Arc<Exec>) -> Agent {
    Agent::new(fx.clone())
        .with_capabilities(Arc::new(Caps))
        .with_executor(exec.clone())
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(reasoner())
}

fn spec(limits: WorkLimits) -> WorkSpec {
    WorkSpec::new(WorkId::new("w"), WorkGoal::new("goal")).with_limits(limits)
}

const LIMITS: WorkLimits = WorkLimits {
    max_turns: 8,
    max_executions: 4,
};

struct Insatiable;

impl LocalWorkPolicy for Insatiable {
    fn propose(&self, v: &WorkView<'_>) -> Option<WorkDecision> {
        Some(WorkDecision::RequestCapability(
            req(&format!("e{}", v.turn)).with_input("n", InputValue::Integer(v.turn as i64)),
        ))
    }
}

struct Reactive;

impl LocalWorkPolicy for Reactive {
    fn propose(&self, v: &WorkView<'_>) -> Option<WorkDecision> {
        match v.observations.last() {
            None => Some(run("r1")),
            Some(o) if o.kind == ObservationKind::ExecutionCompleted => Some(done()),
            Some(o) if o.kind == ObservationKind::ExecutionFailed => Some(WorkDecision::Block {
                reason: "failed".into(),
            }),
            Some(_) => Some(WorkDecision::Escalate {
                reason: "cancelled".into(),
            }),
        }
    }
}

/// Counts the trajectory in the most literal way, independently of `WorkMeasurement::derive`.
fn recount(report: &WorkReport) -> [usize; 9] {
    let mut c = [0; 9];
    for e in &report.events {
        match e {
            WorkEvent::DecisionStarted { .. } => c[0] += 1,
            WorkEvent::Execution(ExecutionEvent::ExecutionStarted { .. }) => c[1] += 1,
            WorkEvent::ObservationRecorded { .. } => c[2] += 1,
            WorkEvent::EvidenceReused { .. } => c[3] += 1,
            WorkEvent::LocalDecision { .. } => c[4] += 1,
            WorkEvent::ModelEscalation { .. } => c[5] += 1,
            WorkEvent::ModelCalled { .. } => c[6] += 1,
            _ => {}
        }
        if let WorkEvent::ModelEscalation { context, .. } = e {
            c[7] += context.bytes;
            c[8] += context.chars;
        }
    }
    c
}

fn add(
    out: &mut Vec<Case>,
    name: &'static str,
    report: WorkReport,
    limits: WorkLimits,
    e: &Arc<Exec>,
    f: &Arc<Fx>,
) {
    out.push(Case {
        name,
        report,
        limits,
        executor_calls: e.calls.load(Ordering::SeqCst),
        model_calls: f.calls.load(Ordering::SeqCst),
    });
}

struct Case {
    name: &'static str,
    report: WorkReport,
    limits: WorkLimits,
    executor_calls: usize,
    model_calls: usize,
}

async fn workloads() -> Vec<Case> {
    let mut out = Vec::new();
    let (fx, ex) = (Fx::new(), Exec::new(ExecutionStatus::Success));
    let r = agent(&fx, &ex)
        .run_work(&spec(LIMITS), &Reactive, &script(vec![]))
        .await;
    add(&mut out, "completion", r, LIMITS, &ex, &fx);

    let (fx, ex) = (Fx::new(), Exec::new(ExecutionStatus::Failure));
    let r = agent(&fx, &ex)
        .run_work(&spec(LIMITS), &Reactive, &script(vec![]))
        .await;
    add(&mut out, "failure", r, LIMITS, &ex, &fx);

    let (fx, ex) = (Fx::new(), Exec::new(ExecutionStatus::Cancelled));
    let r = agent(&fx, &ex)
        .run_work(&spec(LIMITS), &Reactive, &script(vec![]))
        .await;
    add(&mut out, "cancelled", r, LIMITS, &ex, &fx);

    // Existing valid evidence, established by a real prior execution outside the measured work.
    let (fx, ex) = (Fx::new(), Exec::new(ExecutionStatus::Success));
    let a = agent(&fx, &ex);
    a.obtain_evidence(&req("seed")).await.unwrap();
    let seeded = ex.calls.load(Ordering::SeqCst);
    let p = ScriptedPolicy::new(vec![Some(run("again")), Some(done())]);
    let r = a.run_work(&spec(LIMITS), &p, &script(vec![])).await;
    assert_eq!(
        ex.calls.load(Ordering::SeqCst),
        seeded,
        "the measured work executed nothing"
    );
    out.push(Case {
        name: "evidence",
        report: r,
        limits: LIMITS,
        executor_calls: 0,
        model_calls: fx.calls.load(Ordering::SeqCst),
    });

    let limits = WorkLimits {
        max_turns: 6,
        max_executions: 4,
    };
    let (fx, ex) = (Fx::new(), Exec::new(ExecutionStatus::Success));
    let r = agent(&fx, &ex)
        .run_work(&spec(limits), &Insatiable, &script(vec![]))
        .await;
    add(&mut out, "limit", r, limits, &ex, &fx);

    let (fx, ex) = (Fx::new(), Exec::new(ExecutionStatus::Success));
    let p = ScriptedPolicy::new(vec![None, Some(done())]);
    let r = agent(&fx, &ex)
        .run_work(&spec(LIMITS), &p, &script(vec![run("asked")]))
        .await;
    add(&mut out, "escalation", r, LIMITS, &ex, &fx);

    let (fx, ex) = (Fx::failing(), Exec::new(ExecutionStatus::Success));
    let r = agent(&fx, &ex)
        .run_work(
            &spec(LIMITS),
            &ScriptedPolicy::new(vec![None]),
            &script(vec![done()]),
        )
        .await;
    add(&mut out, "provider-failure", r, LIMITS, &ex, &fx);
    out
}

#[tokio::test]
async fn the_measurement_is_exactly_what_the_event_stream_says() {
    for case in workloads().await {
        let report = &case.report;
        let m = report.measurement();
        let c = recount(report);
        let n = case.name;
        assert_eq!(m.turns as usize, c[0], "{n}: turns");
        assert_eq!(m.executions as usize, c[1], "{n}: executions");
        assert_eq!(m.observations as usize, c[2], "{n}: observations");
        assert_eq!(m.evidence_hits as usize, c[3], "{n}: evidence hits");
        assert_eq!(m.local_decisions as usize, c[4], "{n}: local decisions");
        assert_eq!(m.model_escalations as usize, c[5], "{n}: escalations");
        assert_eq!(m.model_calls as usize, c[6], "{n}: model calls");
        // WorkMeasurement == measurement(events)
        assert_eq!(
            m,
            WorkMeasurement::derive(
                report.work_id.clone(),
                report.outcome.clone(),
                &report.events,
                report.latency
            ),
            "{n}"
        );

        // The summary is the same measurement; nothing is counted twice differently.
        let s = report.summary;
        assert_eq!(
            (s.turns, s.executions, s.observations, s.evidence_hits),
            (
                m.turns as usize,
                m.executions as usize,
                m.observations as usize,
                m.evidence_hits as usize
            ),
            "{n}"
        );
        assert_eq!(
            (s.local_decisions, s.model_escalations, s.context_bytes),
            (
                m.local_decisions as usize,
                m.model_escalations as usize,
                m.context_bytes as usize
            ),
            "{n}"
        );
        assert_eq!(s.terminal_state, m.terminal_state(), "{n}");
        assert_eq!(s.elapsed, m.total_latency, "{n}");

        // What the world saw agrees with what the loop reported.
        assert_eq!(
            m.executions as usize, case.executor_calls,
            "{n}: the executor was invoked exactly this often"
        );
        assert_eq!(
            m.model_calls as usize, case.model_calls,
            "{n}: the provider was called exactly this often"
        );
        assert_eq!(
            m.model_calls, m.model_escalations,
            "{n}: one call per escalation"
        );
        assert_eq!(
            m.observations as usize,
            report.observations.len() - m.evidence_hits as usize,
            "{n}: observations of executions"
        );

        // The invariants hold for every canonical-style workload.
        assert_eq!(
            verify_trajectory(&report.events, &case.limits),
            Vec::<String>::new(),
            "{n}"
        );
        assert!(
            m.turns as usize <= case.limits.max_turns
                && m.executions as usize <= case.limits.max_executions,
            "{n}"
        );
        assert!(m.observations <= m.executions, "{n}");
    }
}

#[tokio::test]
async fn the_canonical_shapes_are_what_they_claim_to_be() {
    let all = workloads().await;
    let m = |name: &str| {
        all.iter()
            .find(|c| c.name == name)
            .unwrap()
            .report
            .measurement()
    };
    let completion = m("completion");
    assert_eq!(
        (
            completion.turns,
            completion.executions,
            completion.observations,
            completion.local_decisions
        ),
        (2, 1, 1, 2)
    );
    assert!(matches!(completion.outcome, WorkOutcome::Completed { .. }));
    let failure = m("failure");
    assert_eq!((failure.executions, failure.observations), (1, 1));
    assert!(matches!(failure.outcome, WorkOutcome::Blocked { .. }));
    let evidence = m("evidence");
    assert_eq!(
        (
            evidence.executions,
            evidence.evidence_hits,
            evidence.observations
        ),
        (0, 1, 0)
    );
    let limit = m("limit");
    assert_eq!((limit.turns, limit.executions), (5, 4));
    assert_eq!(
        limit.outcome,
        WorkOutcome::LimitReached {
            limit: LimitKind::Executions
        }
    );
    let escalation = m("escalation");
    assert_eq!(
        (escalation.model_escalations, escalation.model_calls),
        (1, 1)
    );
    assert!(escalation.context_bytes > 0 && escalation.context_chars > 0);
    // Failed provider: the call was made, nothing was reported, and the work failed.
    let failed = m("provider-failure");
    assert_eq!((failed.model_calls, failed.model_tokens), (1, None));
    assert!(matches!(failed.outcome, WorkOutcome::Failed { .. }));
}

#[tokio::test]
async fn context_and_tokens_match_what_the_provider_received_and_reported() {
    let (fx, ex) = (Fx::new(), Exec::new(ExecutionStatus::Failure));
    let p = ScriptedPolicy::new(vec![Some(run("e1")), None, None]);
    let boundary = script(vec![WorkDecision::Block { reason: "b".into() }]);
    let report = agent(&fx, &ex).run_work(&spec(LIMITS), &p, &boundary).await;
    let m = report.measurement();
    let sent = fx.seen.lock().unwrap();
    let bytes: usize = sent
        .iter()
        .flat_map(|r| &r.messages)
        .map(|x| x.content.len())
        .sum();
    let chars: usize = sent
        .iter()
        .flat_map(|r| &r.messages)
        .map(|x| x.content.chars().count())
        .sum();
    assert_eq!(
        (m.context_bytes as usize, m.context_chars as usize),
        (bytes, chars)
    );
    assert_eq!(m.model_calls as usize, sent.len());
    assert_eq!(
        m.model_tokens,
        Some(10 * sent.len() as u64),
        "7 prompt + 3 completion per call, as reported"
    );

    // No call, no usage: an explicit None, not a zero that looks measured.
    let (fx2, ex2) = (Fx::new(), Exec::new(ExecutionStatus::Success));
    let quiet = agent(&fx2, &ex2)
        .run_work(&spec(LIMITS), &Reactive, &script(vec![]))
        .await
        .measurement();
    assert_eq!(
        (quiet.model_calls, quiet.model_tokens, quiet.context_bytes),
        (0, None, 0)
    );
}

#[tokio::test]
async fn latency_is_attributed_to_the_model_and_to_compute_separately() {
    let (fx, ex) = (
        Fx::slow(Duration::from_millis(40)),
        Exec::slow(Duration::from_millis(25)),
    );
    let p = ScriptedPolicy::new(vec![None, Some(done())]);
    let report = agent(&fx, &ex)
        .run_work(&spec(LIMITS), &p, &script(vec![run("a")]))
        .await;
    let m = report.measurement();
    assert!(
        m.model_latency >= Duration::from_millis(40),
        "{:?}",
        m.model_latency
    );
    assert!(
        m.compute_latency >= Duration::from_millis(25),
        "{:?}",
        m.compute_latency
    );
    assert!(
        m.total_latency >= m.model_latency + m.compute_latency,
        "total covers both: {:?}",
        m
    );
    assert!(m.local_decision_latency <= m.total_latency);

    // Nothing is attributed to a boundary that was never crossed.
    let (fx, ex) = (
        Fx::slow(Duration::from_millis(30)),
        Exec::new(ExecutionStatus::Success),
    );
    let local = agent(&fx, &ex)
        .run_work(&spec(LIMITS), &Reactive, &script(vec![]))
        .await
        .measurement();
    assert_eq!(local.model_latency, Duration::ZERO);
    let (fx, ex) = (Fx::new(), Exec::slow(Duration::from_millis(30)));
    let none = agent(&fx, &ex)
        .run_work(
            &spec(LIMITS),
            &ScriptedPolicy::new(vec![Some(done())]),
            &script(vec![]),
        )
        .await
        .measurement();
    assert_eq!(none.compute_latency, Duration::ZERO);
    assert_eq!(none.executions, 0);
}

#[tokio::test]
async fn bounds_hold_including_zero_and_simultaneous_limits() {
    for (max_turns, max_executions) in [(0, 0), (0, 3), (3, 0), (3, 3), (4, 4), (1, 1), (2, 5)] {
        let limits = WorkLimits {
            max_turns,
            max_executions,
        };
        let (fx, ex) = (Fx::new(), Exec::new(ExecutionStatus::Success));
        let report = agent(&fx, &ex)
            .run_work(&spec(limits), &Insatiable, &script(vec![]))
            .await;
        let m = report.measurement();
        assert!(
            m.turns as usize <= max_turns && m.executions as usize <= max_executions,
            "{limits:?}: {m:?}"
        );
        assert!(
            matches!(m.outcome, WorkOutcome::LimitReached { .. }),
            "{limits:?}"
        );
        assert_eq!(ex.calls.load(Ordering::SeqCst), m.executions as usize);
        assert_eq!(
            verify_trajectory(&report.events, &limits),
            Vec::<String>::new(),
            "{limits:?}"
        );
    }
}

#[tokio::test]
async fn model_claims_cannot_create_executions_observations_or_evidence() {
    struct Boasts;
    #[async_trait::async_trait]
    impl ModelProvider for Boasts {
        async fn complete(&self, _: ModelRequest) -> Result<ModelResponse, FxError> {
            Ok(ModelResponse::new(
                "r",
                "It ran and it succeeded.",
                Usage::new(1, 1),
            ))
        }
    }
    let ex = Exec::new(ExecutionStatus::Success);
    let a = Agent::new(Arc::new(Boasts))
        .with_capabilities(Arc::new(Caps))
        .with_executor(ex.clone())
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(reasoner());
    let report = a
        .run_work(
            &spec(LIMITS),
            &ScriptedPolicy::new(vec![None]),
            &script(vec![done()]),
        )
        .await;
    let m = report.measurement();
    assert_eq!(
        (m.executions, m.observations, m.evidence_hits, m.model_calls),
        (0, 0, 0, 1)
    );
    assert!(report.observations.is_empty());
    assert_eq!(a.lookup_evidence(&req("x")), EvidenceLookup::NotFound);
    assert_eq!(ex.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_and_cancelled_executions_are_measured_as_what_they_were() {
    let (fx, ex) = (Fx::new(), Exec::new(ExecutionStatus::Failure));
    let a = agent(&fx, &ex);
    let report = a.run_work(&spec(LIMITS), &Reactive, &script(vec![])).await;
    assert_eq!(
        report.observations[0].kind,
        ObservationKind::ExecutionFailed
    );
    match a.lookup_evidence(&req("z")) {
        EvidenceLookup::Found(o) => assert_eq!(
            o.status,
            ExecutionStatus::Failure,
            "a failure is failure evidence"
        ),
        other => panic!("{other:?}"),
    }

    let (fx, ex) = (Fx::new(), Exec::new(ExecutionStatus::Cancelled));
    let a = agent(&fx, &ex);
    let report = a.run_work(&spec(LIMITS), &Reactive, &script(vec![])).await;
    let m = report.measurement();
    assert_eq!((m.executions, m.observations), (1, 1));
    assert_eq!(
        report.observations[0].kind,
        ObservationKind::ExecutionCancelled
    );
    assert!(
        !matches!(m.outcome, WorkOutcome::Completed { .. }),
        "cancelled is never success"
    );
    assert_eq!(a.lookup_evidence(&req("z")), EvidenceLookup::NotFound);
    assert!(
        !report
            .events
            .iter()
            .any(|e| matches!(e, WorkEvent::EvidenceRecorded { .. }))
    );
}

#[tokio::test]
async fn an_unavailable_executor_is_not_counted_as_an_execution() {
    // The class of mistake the derived measurement exists to catch: nothing started, so nothing counts.
    let fx = Fx::new();
    let a = Agent::new(fx)
        .with_capabilities(Arc::new(Caps))
        .with_observer(Arc::new(ExecutionObserver))
        .with_local_reasoner(reasoner());
    let report = a.run_work(&spec(LIMITS), &Reactive, &script(vec![])).await;
    let m = report.measurement();
    assert!(matches!(m.outcome, WorkOutcome::Blocked { .. }));
    assert_eq!((m.executions, m.observations), (0, 0));
    assert_eq!(report.summary.executions, 0);
}

#[tokio::test]
async fn the_trajectory_checker_catches_what_it_is_there_to_catch() {
    let all = workloads().await;
    let good = all
        .iter()
        .find(|c| c.name == "completion")
        .unwrap()
        .report
        .events
        .clone();
    assert!(verify_trajectory(&good, &LIMITS).is_empty());
    let bad = |events: Vec<WorkEvent>| !verify_trajectory(&events, &LIMITS).is_empty();

    // Two terminal events.
    let mut twice = good.clone();
    twice.push(WorkEvent::WorkFailed {
        work_id: WorkId::new("w"),
        reason: "x".into(),
    });
    assert!(bad(twice));
    // No terminal event.
    assert!(bad(good[..good.len() - 1].to_vec()));
    // An observation with no execution behind it.
    let no_exec: Vec<_> = good
        .iter()
        .filter(|e| {
            !matches!(
                e,
                WorkEvent::Execution(ExecutionEvent::ExecutionStarted { .. })
            )
        })
        .cloned()
        .collect();
    assert!(bad(no_exec));
    // Evidence with no observation behind it.
    let no_obs: Vec<_> = good
        .iter()
        .filter(|e| !matches!(e, WorkEvent::ObservationRecorded { .. }))
        .cloned()
        .collect();
    assert!(bad(no_obs));
    // Evidence reused in a turn that also executed.
    let mut reused = good.clone();
    let at = reused
        .iter()
        .position(|e| matches!(e, WorkEvent::ObservationRecorded { .. }))
        .unwrap();
    reused.insert(
        at,
        WorkEvent::EvidenceReused {
            work_id: WorkId::new("w"),
            turn: 0,
            capability: CapabilityId::new("compute.selftest").unwrap(),
            receipt_id: None,
        },
    );
    assert!(bad(reused));
    // Two executions in one turn.
    let mut double = good.clone();
    let at = double
        .iter()
        .position(|e| {
            matches!(
                e,
                WorkEvent::Execution(ExecutionEvent::ExecutionStarted { .. })
            )
        })
        .unwrap();
    double.insert(at, double[at].clone());
    assert!(bad(double));
    // Beyond the limits.
    assert!(
        !verify_trajectory(
            &good,
            &WorkLimits {
                max_turns: 1,
                max_executions: 4
            }
        )
        .is_empty()
    );
    assert!(
        !verify_trajectory(
            &good,
            &WorkLimits {
                max_turns: 8,
                max_executions: 0
            }
        )
        .is_empty()
    );
    // An escalation without its model call.
    let esc = all
        .iter()
        .find(|c| c.name == "escalation")
        .unwrap()
        .report
        .events
        .clone();
    assert!(verify_trajectory(&esc, &LIMITS).is_empty());
    let no_call: Vec<_> = esc
        .iter()
        .filter(|e| !matches!(e, WorkEvent::ModelCalled { .. }))
        .cloned()
        .collect();
    assert!(bad(no_call));
}

fn without_latency(mut m: WorkMeasurement) -> WorkMeasurement {
    m.model_latency = Duration::ZERO;
    m.compute_latency = Duration::ZERO;
    m.local_decision_latency = Duration::ZERO;
    m.total_latency = Duration::ZERO;
    m
}

#[tokio::test]
async fn identical_workloads_give_identical_structure_counts_and_outcome() {
    let (a, b) = (workloads().await, workloads().await);
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(x.report.events, y.report.events, "{}", x.name);
        assert_eq!(
            without_latency(x.report.measurement()),
            without_latency(y.report.measurement()),
            "{}",
            x.name
        );
        assert_eq!(x.report.outcome, y.report.outcome, "{}", x.name);
    }
}

#[tokio::test]
async fn the_trace_is_structural_and_carries_no_prompt() {
    let all = workloads().await;
    let esc = all.iter().find(|c| c.name == "escalation").unwrap();
    let trace = esc.report.trace();
    assert_eq!(trace.len(), 2);
    assert_eq!(
        (
            trace[0].decision,
            trace[0].source,
            trace[0].model_called,
            trace[0].executed
        ),
        (
            Some("RequestCapability"),
            Some(DecisionSource::Model),
            true,
            true
        )
    );
    assert_eq!(trace[0].receipt_present, Some(true));
    assert!(trace[0].observed && trace[0].evidence_recorded && trace[0].context.is_some());
    assert_eq!(
        (trace[1].decision, trace[1].source, trace[1].executed),
        (Some("Complete"), Some(DecisionSource::Local), false)
    );
    assert!(format!("{trace:?}").find("Goal:").is_none());

    let evidence = all
        .iter()
        .find(|c| c.name == "evidence")
        .unwrap()
        .report
        .trace();
    assert!(evidence[0].evidence_reused && !evidence[0].executed && !evidence[0].observed);
    assert_eq!(evidence[0].receipt_present, Some(true));
}
