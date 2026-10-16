//! Section `fx`: the deterministic Chip + FX measurement.
//!
//! The real work loop (`Agent::run_work`), the real model port adapter (`FxModelProvider`) and the
//! real FX executor (`fx_core::execute` over `fx_single_attempt`) run end to end. Only the two host
//! edges are doubles: a scripted `ProviderPort` under FX (no network, no credentials, no local model)
//! and an in-process capability under Chip. Nothing here is a second runtime and nothing in the
//! product was changed to make it measurable.
//!
//! # Timing boundaries
//!
//! Every boundary is read from a monotonic clock (`Instant`) at a point the harness wraps, and the
//! boundaries are one chronological partition of the run, so they add up to the total exactly (that
//! identity is asserted; no timing threshold is). Marks:
//!
//! ```text
//! submit ─ B1 ─ capabilities() #1 ─ B2 ─ complete() enter ─ B3 ─ port.invoke ─ B4 ─ first poll item
//!   ─ B5 ─ complete() exit ─ B6 ─ executor.execute enter ─ B7 ─ executor.execute exit ─ (B2 again) …
//!   … last complete() exit ─ B6 ─ work outcome returned          B8 = submit → outcome = B1+…+B7
//! ```
//!
//! * **B1** submission to agent-loop start. The loop emits no timestamped start event, so its first
//!   observable act (the first capability discovery) stands in. The true loop start is *unavailable*
//!   and is reported as such; B1 is named for what it measures.
//! * **B2** loop processing before inference (context, validation, request build), summed over the run.
//! * **B3** FX invocation to provider dispatch: `complete()` entry to `ProviderPort::invoke`
//!   (request mapping, FX validation, attempt setup).
//! * **B4** provider dispatch to the first response event: `invoke` to the first poll that yields an
//!   item. With the scripted port this is the harness's own cost, not a network time.
//! * **B5** first response event to the terminal inference outcome: through FX normalization and the
//!   mapping back to a Chip response, up to `complete()` returning.
//! * **B6** loop processing after inference (decision parse and checks, observation, events).
//! * **B7** tool execution (the in-process capability: dispatch cost without execution cost).
//! * **B8** total, submission to terminal work outcome.
//!
//! Each mark takes one uncontended mutex lock and a `Vec` push (tens of nanoseconds); the
//! `uninstrumented` scenario shows what that costs by running the same turn without marks.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chip_core::{
    Agent, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityProvider, ExecutionError, ExecutionObserver, ExecutionRequest, ExecutionResult,
    ExecutionStatus, Executor, ModelDecisionBoundary, NoLocalPolicy, WorkGoal, WorkId, WorkLimits,
    WorkOutcome, WorkSpec,
};
use chip_fx::{FxModelProvider, OptionsPolicy, RequestIds};
use chip_inference::{FxError, ModelProvider, ModelRequest, ModelResponse};
use fx_core::{AttemptBudget, Request};
use fx_single_attempt::testing::{done, poll};
use fx_single_attempt::{
    AbortReport, Observed, PortFailure, PortItem, PortPoll, PortSupport, ProviderPort, Terminal,
};

use super::{COMPLETE, NOOP, Opts, Report, request_json};

/// Turns run and discarded before the measured ones in each scenario.
const WARMUP: usize = 10;

// ---- marks -----------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mark {
    Capabilities,
    CompleteEnter,
    Invoke,
    FirstItem,
    CompleteExit,
    ExecStart,
    ExecEnd,
}

type Marks = Vec<(Mark, Instant)>;

/// The marks of one run, or `None` for the uninstrumented variant.
#[derive(Clone)]
struct Timeline(Option<Arc<Mutex<Marks>>>);

impl Timeline {
    fn on() -> Self {
        Timeline(Some(Arc::new(Mutex::new(Vec::with_capacity(512)))))
    }
    fn off() -> Self {
        Timeline(None)
    }
    #[inline]
    fn mark(&self, mark: Mark) {
        if let Some(t) = &self.0 {
            let now = Instant::now();
            t.lock().unwrap().push((mark, now));
        }
    }
    fn take(&self) -> Marks {
        self.0
            .as_ref()
            .map(|t| std::mem::take(&mut *t.lock().unwrap()))
            .unwrap_or_default()
    }
}

// ---- the scripted provider port --------------------------------------------------------------------

/// Serves one scripted reply per FX attempt: the text in one poll, then the provider's terminal
/// marker in the next, so "first response event" and "terminal outcome" are distinct events. It
/// never waits: any elapsed time in a run is the code under test, not a sleep.
struct ScriptedPort {
    replies: VecDeque<String>,
    timeline: Timeline,
    invokes: Arc<AtomicUsize>,
}

struct ScriptedCall {
    polls: VecDeque<PortPoll>,
    seen_item: bool,
}

impl ProviderPort for ScriptedPort {
    type Call = ScriptedCall;

    fn support(&self) -> PortSupport {
        // What Chip's work loop asks for: structured JSON output and generation options.
        PortSupport {
            response_format: true,
            generation_options: true,
            ..PortSupport::default()
        }
    }

    fn invoke(
        &mut self,
        _request: &Request,
        _budget: Option<AttemptBudget>,
    ) -> Result<ScriptedCall, PortFailure> {
        self.timeline.mark(Mark::Invoke);
        self.invokes.fetch_add(1, Ordering::SeqCst);
        let text = self
            .replies
            .pop_front()
            .expect("the port was invoked more times than the scenario scripted");
        Ok(ScriptedCall {
            polls: VecDeque::from([
                poll(vec![PortItem::Text(text)], Observed::OutputObserved, false),
                poll(
                    vec![done(Some("stop"), Terminal::ProviderMarker)],
                    Observed::OutputObserved,
                    true,
                ),
            ]),
            seen_item: false,
        })
    }

    fn poll(
        &mut self,
        call: &mut ScriptedCall,
        _max_items: usize,
    ) -> Result<PortPoll, PortFailure> {
        let next = call
            .polls
            .pop_front()
            .unwrap_or_else(|| poll(Vec::new(), Observed::Invoked, true));
        if !call.seen_item && !next.items.is_empty() {
            call.seen_item = true;
            self.timeline.mark(Mark::FirstItem);
        }
        Ok(next)
    }

    fn abort(&mut self, _call: &mut ScriptedCall) -> AbortReport {
        AbortReport {
            transport_closed: false,
        }
    }

    fn release(&mut self, _call: ScriptedCall) {}
}

// ---- the wrappers (harness only) -------------------------------------------------------------------

/// Marks the two ends of `complete()`; everything between is Chip's adapter and FX.
struct TimedModel {
    inner: Arc<FxModelProvider<ScriptedPort>>,
    timeline: Timeline,
}

#[async_trait::async_trait]
impl ModelProvider for TimedModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, FxError> {
        self.timeline.mark(Mark::CompleteEnter);
        let out = self.inner.complete(request).await;
        self.timeline.mark(Mark::CompleteExit);
        out
    }
}

/// The in-process capability (`bench.noop`), marking discovery and execution.
struct TimedCaps {
    timeline: Timeline,
    executed: AtomicUsize,
}

#[async_trait::async_trait]
impl CapabilityProvider for TimedCaps {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        self.timeline.mark(Mark::Capabilities);
        Ok(vec![
            CapabilityDescriptor::new(
                CapabilityId::new(NOOP)?,
                "Noop",
                "An in-process capability that does nothing",
            )
            .without_evidence_reuse(),
        ])
    }
    async fn availability(&self, _id: &CapabilityId) -> CapabilityAvailability {
        CapabilityAvailability::Available
    }
}

#[async_trait::async_trait]
impl Executor for TimedCaps {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        self.timeline.mark(Mark::ExecStart);
        let n = self.executed.fetch_add(1, Ordering::SeqCst);
        let result = ExecutionResult {
            id: request.id,
            status: ExecutionStatus::Success,
            output: "ok".into(),
            receipt_id: Some(format!("sha256:{n}")),
            evidence: None,
        };
        self.timeline.mark(Mark::ExecEnd);
        Ok(result)
    }
}

// ---- one run ---------------------------------------------------------------------------------------

/// What one turn produced.
struct Run {
    /// Submission to terminal outcome, measured around `run_work` (B8).
    total: Duration,
    /// B1..B7 summed over the run, or `None` when the marks were not a well-formed sequence.
    phases: Option<[Duration; 7]>,
    /// One value per loop iteration (consecutive `complete()` entries), for the per-step series.
    iterations: Vec<Duration>,
}

const PHASES: [(&str, &str); 7] = [
    (
        "b1_submit_to_loop_start",
        "submission to the first capability discovery (stands in for agent-loop start)",
    ),
    (
        "b2_loop_before_inference",
        "loop processing before inference, summed",
    ),
    (
        "b3_fx_invocation_to_dispatch",
        "complete() entry to provider dispatch, summed",
    ),
    (
        "b4_dispatch_to_first_event",
        "provider dispatch to first response event, summed",
    ),
    (
        "b5_first_event_to_terminal",
        "first response event to terminal inference outcome, summed",
    ),
    (
        "b6_loop_after_inference",
        "loop processing after inference, summed",
    ),
    ("b7_tool_execution", "tool execution, summed"),
];

/// Partitions the run chronologically: each span between two marks is spent in the region the loop
/// was in. `None` if the marks are not in the order the boundaries assume (a harness or loop change):
/// the boundaries are then reported unavailable, never guessed.
fn partition(submit: Instant, end: Instant, marks: &[(Mark, Instant)]) -> Option<[Duration; 7]> {
    // Regions, as indexes into the result: 1 pre-inference, 2 FX before dispatch, 3 dispatch to first
    // event, 4 first event to terminal, 5 post-inference, 6 tool execution. Index 0 is B1.
    let (first, at) = *marks.first()?;
    if first != Mark::Capabilities {
        return None;
    }
    let mut out = [Duration::ZERO; 7];
    out[0] = at.checked_duration_since(submit)?;
    let (mut region, mut prev) = (1usize, at);
    for &(mark, at) in &marks[1..] {
        out[region] += at.checked_duration_since(prev)?;
        prev = at;
        region = match (region, mark) {
            (1 | 5, Mark::Capabilities) => region,
            (1, Mark::CompleteEnter) => 2,
            (2, Mark::Invoke) => 3,
            (3, Mark::FirstItem) => 4,
            (4, Mark::CompleteExit) => 5,
            (5, Mark::ExecStart) => 6,
            (6, Mark::ExecEnd) => 1,
            _ => return None,
        };
    }
    out[region] += end.checked_duration_since(prev)?;
    Some(out)
}

/// Spans between consecutive `complete()` entries: one loop iteration each (inference, decision
/// handling, execution, preparation of the next request).
fn iterations(marks: &[(Mark, Instant)]) -> Vec<Duration> {
    let enters: Vec<Instant> = marks
        .iter()
        .filter(|(m, _)| *m == Mark::CompleteEnter)
        .map(|(_, t)| *t)
        .collect();
    enters.windows(2).map(|w| w[1] - w[0]).collect()
}

/// The reply script of a turn with `steps` tool requests and a final completion.
fn script(steps: usize) -> Vec<String> {
    let mut replies: Vec<String> = (0..steps).map(|_| request_json(NOOP, "")).collect();
    replies.push(COMPLETE.into());
    replies
}

/// One complete turn through Chip, FX and the scripted provider. Asserts the counts that make the
/// numbers meaningful (one provider invocation per decision, one execution per tool request, a
/// completed outcome, no retry); it asserts no time.
async fn turn(steps: usize, timeline: Timeline) -> Run {
    let invokes = Arc::new(AtomicUsize::new(0));
    let port = ScriptedPort {
        replies: script(steps).into(),
        timeline: timeline.clone(),
        invokes: invokes.clone(),
    };
    let fx = FxModelProvider::new(
        port,
        RequestIds::new(*b"chip-bnc"),
        OptionsPolicy::DropUnsupported,
    )
    .with_json_object_output();
    let model = Arc::new(TimedModel {
        inner: Arc::new(fx),
        timeline: timeline.clone(),
    });
    let caps = Arc::new(TimedCaps {
        timeline: timeline.clone(),
        executed: AtomicUsize::new(0),
    });
    let agent = Agent::with_model(model, "scripted")
        .with_capabilities(caps.clone())
        .with_executor(caps.clone())
        .with_observer(Arc::new(ExecutionObserver));
    let spec = WorkSpec::new(WorkId::new("fx-bench"), WorkGoal::new("baseline"))
        .with_limits(WorkLimits {
            max_turns: steps + 2,
            max_executions: steps + 1,
        })
        .with_evidence_reuse_prohibited(CapabilityId::new(NOOP).unwrap());

    let submit = Instant::now();
    let report = agent
        .run_work(&spec, &NoLocalPolicy, &ModelDecisionBoundary)
        .await;
    let end = Instant::now();

    assert!(
        matches!(report.outcome, WorkOutcome::Completed { .. }),
        "{:?}",
        report.outcome
    );
    assert_eq!(
        invokes.load(Ordering::SeqCst),
        steps + 1,
        "one provider invocation per decision, no retry"
    );
    assert_eq!(
        caps.executed.load(Ordering::SeqCst),
        steps,
        "one execution per tool request"
    );

    let marks = timeline.take();
    let phases = partition(submit, end, &marks);
    if let Some(p) = &phases {
        // The identity that makes the boundaries trustworthy: no gap, no double count.
        assert_eq!(
            p.iter().sum::<Duration>(),
            end - submit,
            "boundaries must partition the total"
        );
    }
    Run {
        total: end - submit,
        phases,
        iterations: iterations(&marks),
    }
}

// ---- the section -----------------------------------------------------------------------------------

/// Records one scenario's total and boundaries. `Report` computes median, p95, min and max.
fn record(report: &mut Report, id: &str, steps: usize, runs: &[Run]) {
    let n = runs.len();
    let totals: Vec<Duration> = runs.iter().map(|r| r.total).collect();
    let wall: f64 = totals.iter().map(Duration::as_secs_f64).sum();
    let note = format!("{} inferences, {steps} tool executions per turn", steps + 1);
    report.series(
        "L0",
        &format!("{id}.b8_total"),
        &format!("{id}: B8 submission to terminal outcome"),
        totals,
        &note,
        Some((n as f64 / wall, "turns/s")),
    );
    let complete: Vec<&Run> = runs.iter().filter(|r| r.phases.is_some()).collect();
    for (i, (key, what)) in PHASES.iter().enumerate() {
        let sid = format!("{id}.{key}");
        if complete.len() < n {
            report.unavailable(
                &sid,
                &format!(
                    "{} of {n} runs had marks out of the expected order",
                    n - complete.len()
                ),
            );
            continue;
        }
        report.series(
            "L0",
            &sid,
            &format!("{id}: {key}"),
            complete.iter().map(|r| r.phases.unwrap()[i]).collect(),
            what,
            None,
        );
    }
}

pub async fn fx(report: &mut Report, opts: &Opts) {
    report.unavailable(
        "fx.agent_loop_start",
        "the work loop emits no timestamped start event; B1 ends at the first capability discovery instead",
    );
    let scenarios: [(&str, usize, usize); 4] = [
        ("fx.turn.no_tool", 0, 2000),
        ("fx.turn.tool_round_trip", 1, 2000),
        ("fx.turn.steps.10", 10, 1000),
        ("fx.turn.steps.50", 50, 300),
    ];
    for (id, steps, full) in scenarios {
        let n = opts.samples(full);
        for _ in 0..WARMUP {
            turn(steps, Timeline::on()).await;
        }
        let mut runs = Vec::with_capacity(n);
        for _ in 0..n {
            runs.push(turn(steps, Timeline::on()).await);
        }
        report.scenario(
            id,
            serde_json::json!({
                "kind": "direct-microbenchmark",
                "tool_steps": steps,
                "inferences": steps + 1,
                "provider": "scripted ProviderPort, no network, no sleeps",
                "capability": NOOP,
                "warmup": WARMUP,
                "iterations": n,
            }),
        );
        if steps >= 10 {
            let per: Vec<Duration> = runs
                .iter()
                .flat_map(|r| r.iterations.iter().copied())
                .collect();
            let secs: f64 = per.iter().map(Duration::as_secs_f64).sum();
            let rate = per.len() as f64 / secs;
            report.series(
                "L0",
                &format!("fx.step.per_iteration.{steps}"),
                &format!("{id}: one loop iteration (inference, decision, tool, next request)"),
                per,
                "span between consecutive complete() entries",
                Some((rate, "steps/s")),
            );
        }
        record(report, id, steps, &runs);
    }

    // The same 50-step turn with no marks: what the instrumentation itself costs.
    let n = opts.samples(300);
    for _ in 0..WARMUP {
        turn(50, Timeline::off()).await;
    }
    let mut plain = Vec::with_capacity(n);
    for _ in 0..n {
        plain.push(turn(50, Timeline::off()).await.total);
    }
    let wall: f64 = plain.iter().map(Duration::as_secs_f64).sum();
    report.scenario(
        "fx.turn.steps.50.uninstrumented",
        serde_json::json!({
            "kind": "direct-microbenchmark", "tool_steps": 50, "inferences": 51,
            "marks": false, "warmup": WARMUP, "iterations": n,
        }),
    );
    report.series(
        "L0",
        "fx.turn.steps.50.uninstrumented.b8_total",
        "fx.turn.steps.50.uninstrumented: wall total, no marks",
        plain,
        "compare with fx.turn.steps.50.b8_total for the cost of the marks",
        Some((n as f64 / wall, "turns/s")),
    );

    // Independent turns in parallel: each has its own provider, agent and capability. Workers are
    // tokio tasks on the multi-thread runtime; with fewer cores than workers they share cores.
    for workers in [1usize, 4, 16] {
        let id = format!("fx.concurrent.{workers}");
        let batches = opts.samples(300);
        let go = |workers: usize| async move {
            let t = Instant::now();
            let handles: Vec<_> = (0..workers)
                .map(|_| tokio::spawn(turn(10, Timeline::off())))
                .collect();
            let mut each = Vec::with_capacity(workers);
            for h in handles {
                each.push(h.await.unwrap().total);
            }
            (t.elapsed(), each)
        };
        for _ in 0..WARMUP / 2 {
            go(workers).await;
        }
        let (mut walls, mut turns) = (Vec::new(), Vec::new());
        for _ in 0..batches {
            let (wall, each) = go(workers).await;
            walls.push(wall);
            turns.extend(each);
        }
        let secs: f64 = walls.iter().map(Duration::as_secs_f64).sum();
        let throughput = (batches * workers) as f64 / secs;
        report.scenario(
            &id,
            serde_json::json!({
                "kind": "direct-microbenchmark", "workers": workers, "tool_steps_per_turn": 10,
                "warmup_batches": WARMUP / 2, "iterations": batches,
            }),
        );
        report.series(
            "L0",
            &format!("{id}.batch_wall"),
            &format!("{id}: wall time of {workers} parallel 10-step turns"),
            walls,
            "batch start to the last turn finishing",
            Some((throughput, "turns/s")),
        );
        report.series(
            "L0",
            &format!("{id}.turn_latency"),
            &format!("{id}: latency of one turn while {workers} run in parallel"),
            turns,
            "each turn's own submission to outcome",
            None,
        );
    }
}
