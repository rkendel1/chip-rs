//! `chip serve [--host H] [--port P]`: the Chip Runtime Service.
//!
//! A thin HTTP surface over the one work runtime ([`WorkRuntime`]) that `chip work` also uses. It
//! contains no agent logic: it accepts a goal, allocates a Chip-owned work id, starts the existing
//! loop in the background, and projects what the runtime reports. The HTTP layer never executes,
//! observes, records evidence for, or completes anything.
//!
//! **Local trusted-client interface.** Chip Runtime Service is currently a local trusted-client
//! interface. It binds to loopback by default, has no authentication and no CORS, and is not a
//! remote or multi-user service. Remote exposure and authentication are intentionally out of scope.
//!
//! **In memory only.** The work registry lives for the life of the process. Nothing is persisted;
//! when the process exits, every work item and its events are gone.
//!
//! Routes (all JSON):
//!
//! - `GET  /health`
//! - `POST /v1/work` `{"goal": "...", "kind": "change"|"verify"|"inspect" (optional)}` -> 202 `{"work_id", "status": "running"}`
//! - `GET  /v1/work/{id}`
//! - `GET  /v1/work/{id}/events`
//! - `POST /v1/work/{id}/cancel`
//!
//! The client supplies a goal, and optionally how completion is judged (`kind`), and nothing else: provider, model, endpoint, executable, workspace
//! root, ids, receipts, observations and evidence are all runtime concerns, and a request that
//! names any of them is refused, not ignored.
//!
//! Exit status: 0 normal end (the service runs until it is stopped), 2 usage, 3 required
//! infrastructure unavailable (no model selected, no usable PAX; nothing ran), 4 could not listen.

use std::collections::{HashMap, VecDeque};
use std::io::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chip_core::{
    CapabilityEvent, EnvironmentDescription, EnvironmentError, Environments, ExecutionEvent,
    WorkEvent, WorkId, WorkLimits,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::local_environment::LocalEnvironmentProvider;
use crate::software_work::{
    DEFAULT_MAX_EXECUTIONS, DEFAULT_MAX_TURNS, GoalKind, RunControl, SoftwareWork, WorkRuntime,
    goal_is_acceptable, render_json,
};
use crate::verify::{EXIT_RUNTIME_FAILURE, EXIT_UNAVAILABLE, EXIT_USAGE};

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8765;

const MAX_HEAD_BYTES: usize = 8 * 1024;
/// A goal is at most 2000 bytes; this leaves room for the JSON around it and nothing more.
const MAX_BODY_BYTES: usize = 8 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

// ---- the registry and the scheduler ---------------------------------------------------------------

/// Work admission limits. Both bounds are about work trajectories, not HTTP requests.
pub const DEFAULT_MAX_CONCURRENT_WORK: usize = 2;
pub const DEFAULT_MAX_QUEUED_WORK: usize = 32;
const MAX_CONCURRENT_CEILING: usize = 64;
const MAX_QUEUED_CEILING: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    pub max_concurrent: usize,
    pub max_queued: usize,
}

impl Default for Capacity {
    fn default() -> Self {
        Self {
            max_concurrent: DEFAULT_MAX_CONCURRENT_WORK,
            max_queued: DEFAULT_MAX_QUEUED_WORK,
        }
    }
}

/// The scheduler's own knowledge of one work item: where it is in admission. The agent's lifecycle
/// is the runtime's and is reported separately; `Running` says only that the loop was started.
/// While it runs there is nothing authoritative to show: the loop returns its events and outcome
/// when it ends.
enum Phase {
    Queued,
    Running { admitted: Instant },
    Finished(Box<Finished>),
}

/// What is known once the work has ended, projected once.
struct Finished {
    /// `TerminalState::name()`; `failed` if the task running the loop ended abnormally; or
    /// `cancelled`, which only the scheduler can establish: the work was removed from the queue
    /// before it started.
    state: &'static str,
    /// The loop was started. False only for work cancelled while queued.
    ran: bool,
    /// The CLI's JSON report for this work (`render_json`), unchanged.
    result: Value,
    events: Vec<Value>,
    queue_wait: Duration,
    first_model_call: Option<Duration>,
    work_duration: Option<Duration>,
}

struct Item {
    goal: String,
    /// How completion is judged. The client's choice, like the goal; never the model's.
    kind: GoalKind,
    /// The opaque id of the environment this work acquired; unset until it has one. Never a path.
    environment: Mutex<Option<String>>,
    submitted: Instant,
    control: Arc<RunControl>,
    phase: Mutex<Phase>,
}

/// FIFO admission. Nothing else: no priorities, no stealing, no dependencies.
#[derive(Default)]
struct Scheduler {
    queue: VecDeque<String>,
    active: usize,
}

/// Locks, ignoring poison: a panic in one work's task must not wedge the scheduler.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 1_000_000.0).round() / 1000.0
}

pub struct Service {
    runtime: Arc<WorkRuntime>,
    environments: Arc<Environments>,
    limits: WorkLimits,
    capacity: Capacity,
    /// Lock order: `sched`, then `items` or one item's `phase`. `items` is never held while
    /// taking either.
    sched: Mutex<Scheduler>,
    items: Mutex<HashMap<String, Arc<Item>>>,
    rejected: AtomicU64,
    issued: AtomicU64,
    /// Only a loopback listener can check `Host`: a client that names another host is a browser
    /// being steered to this port by a name that is not ours.
    loopback_hosts_only: bool,
}

impl Service {
    /// Fails closed if more work may run at once than there are isolated environments for it.
    pub fn new(
        runtime: Arc<WorkRuntime>,
        environments: Arc<Environments>,
        loopback_hosts_only: bool,
        capacity: Capacity,
    ) -> Result<Arc<Self>, String> {
        let isolated = environments.isolation_capacity();
        if capacity.max_concurrent > isolated {
            return Err(format!(
                "concurrent work requires isolated environments: {} may run at once but the environment provides {isolated}",
                capacity.max_concurrent
            ));
        }
        Ok(Arc::new(Self {
            runtime,
            environments,
            limits: WorkLimits {
                max_turns: DEFAULT_MAX_TURNS,
                max_executions: DEFAULT_MAX_EXECUTIONS,
            },
            capacity,
            sched: Mutex::new(Scheduler::default()),
            items: Mutex::new(HashMap::new()),
            rejected: AtomicU64::new(0),
            issued: AtomicU64::new(0),
            loopback_hosts_only,
        }))
    }

    /// A Chip-owned id: never a client's, a provider's or a model's.
    fn allocate_id(&self) -> String {
        loop {
            let n = self.issued.fetch_add(1, Ordering::SeqCst);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let mut hash = Sha256::new();
            hash.update(n.to_le_bytes());
            hash.update(now.to_le_bytes());
            hash.update(std::process::id().to_le_bytes());
            let id = format!("work_{}", hex(&hash.finalize()[..8]));
            if !lock(&self.items).contains_key(&id) {
                return id;
            }
        }
    }

    fn item(&self, id: &str) -> Option<Arc<Item>> {
        lock(&self.items).get(id).cloned()
    }

    /// Registers and enqueues. Returns the id and whether admission was immediate.
    fn submit(self: &Arc<Self>, goal: String, kind: GoalKind) -> Result<(String, bool), Response> {
        let mut sched = lock(&self.sched);
        if sched.active >= self.capacity.max_concurrent
            && sched.queue.len() >= self.capacity.max_queued
        {
            self.rejected.fetch_add(1, Ordering::SeqCst);
            return Err(Response::error(
                429,
                "queue_full",
                "The work queue is full; nothing was accepted",
            ));
        }
        let id = self.allocate_id();
        let item = Arc::new(Item {
            goal,
            kind,
            environment: Mutex::new(None),
            submitted: Instant::now(),
            control: Arc::new(RunControl::default()),
            phase: Mutex::new(Phase::Queued),
        });
        lock(&self.items).insert(id.clone(), item.clone());
        sched.queue.push_back(id.clone());
        self.admit_available(&mut sched);
        let admitted = !matches!(*lock(&item.phase), Phase::Queued);
        Ok((id, admitted))
    }

    /// Starts queued work, oldest first, while there is capacity. Called with the scheduler locked
    /// whenever the queue or the active count changes.
    fn admit_available(self: &Arc<Self>, sched: &mut Scheduler) {
        while sched.active < self.capacity.max_concurrent {
            let Some(id) = sched.queue.pop_front() else {
                break;
            };
            let Some(item) = self.item(&id) else { continue };
            sched.active += 1;
            *lock(&item.phase) = Phase::Running {
                admitted: Instant::now(),
            };
            self.spawn(id, item);
        }
    }

    /// Runs the existing loop for one admitted item. Each item gets its own task, its own agent
    /// (built inside `run`) and its own control; the only shared object is the immutable
    /// `WorkRuntime`.
    fn spawn(self: &Arc<Self>, id: String, item: Arc<Item>) {
        let runtime = self.runtime.clone();
        let environments = self.environments.clone();
        let (limits, control, goal, kind) = (
            self.limits,
            item.control.clone(),
            item.goal.clone(),
            item.kind,
        );
        let work_id = WorkId::new(id);
        let owner = item.clone();
        let task = tokio::spawn(async move {
            // One environment for the whole trajectory, acquired before anything else happens: if
            // there is none, there is no model call, no execution and no observation.
            let owned = environments.acquire(work_id.clone()).await?;
            *lock(&owner.environment) = Some(owned.id().to_string());
            let work = runtime
                .run(
                    work_id,
                    &goal,
                    kind,
                    limits,
                    Some(control),
                    owned.environment(),
                )
                .await
                .0;
            let description = owned.environment().description();
            drop(owned);
            Ok::<_, EnvironmentError>((work, description))
        });
        let service = self.clone();
        // The inner task is the containment boundary: a panic in one work ends that work as
        // `failed`, releases its environment (the owner is dropped on unwind) and frees its slot.
        // It never reaches the scheduler, another work, or the server.
        tokio::spawn(async move {
            let outcome = task.await;
            let ended = Instant::now();
            let admitted = match &*lock(&item.phase) {
                Phase::Running { admitted } => *admitted,
                _ => ended,
            };
            let mut finished = match outcome {
                Ok(Ok((work, description))) => finish(&work, &description),
                // Never ran: the agent has no lifecycle and nothing was asked or executed.
                Ok(Err(error)) => Finished {
                    state: "failed",
                    ran: false,
                    result: json!({"outcome_reason": error.to_string()}),
                    events: Vec::new(),
                    queue_wait: Duration::ZERO,
                    first_model_call: None,
                    work_duration: None,
                },
                Err(_) => Finished {
                    state: "failed",
                    ran: true,
                    result: json!({
                        "outcome_reason": "the task running the work ended abnormally; its trajectory is not available",
                    }),
                    events: Vec::new(),
                    queue_wait: Duration::ZERO,
                    first_model_call: None,
                    work_duration: None,
                },
            };
            finished.queue_wait = admitted.saturating_duration_since(item.submitted);
            finished.work_duration = Some(ended.saturating_duration_since(admitted));
            finished.first_model_call = item
                .control
                .first_model_call()
                .map(|t| t.saturating_duration_since(admitted));
            let mut sched = lock(&service.sched);
            *lock(&item.phase) = Phase::Finished(Box::new(finished));
            sched.active -= 1;
            service.admit_available(&mut sched);
        });
    }

    // ---- routing ----------------------------------------------------------------------------------

    async fn handle(self: &Arc<Self>, request: Request) -> Response {
        if self.loopback_hosts_only && !host_is_loopback(request.host.as_deref()) {
            return Response::error(
                403,
                "host_not_allowed",
                "This service answers on loopback only",
            );
        }
        let path = request.path.split('?').next().unwrap_or_default();
        let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        match (segments.as_slice(), request.method.as_str()) {
            (["health"], "GET") => Response::ok(200, json!({"status": "ok"})),
            (["v1", "metrics"], "GET") => self.metrics(),
            (["v1", "work"], "POST") => self.post_work(&request),
            (["v1", "work", id], "GET") => self.get_work(id),
            (["v1", "work", id, "events"], "GET") => self.get_events(id),
            (["v1", "work", id, "cancel"], "POST") => self.cancel(id),
            (["health"], _)
            | (["v1", "metrics"], _)
            | (["v1", "work"], _)
            | (["v1", "work", _], _)
            | (["v1", "work", _, "events" | "cancel"], _) => Response::error(
                405,
                "method_not_allowed",
                "That method is not supported on this route",
            ),
            _ => Response::error(404, "not_found", "Unknown route"),
        }
    }

    fn post_work(self: &Arc<Self>, request: &Request) -> Response {
        let json_type = request
            .content_type
            .as_deref()
            .and_then(|t| t.split(';').next())
            .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"));
        if !json_type {
            return Response::error(415, "unsupported_media_type", "Send application/json");
        }
        let (goal, kind) = match parse_goal(&request.body) {
            Ok(parsed) => parsed,
            Err(response) => return response,
        };
        match self.submit(goal, kind) {
            Ok((id, admitted)) => Response::ok(
                202,
                json!({"work_id": id, "status": if admitted { "running" } else { "queued" }}),
            ),
            Err(response) => response,
        }
    }

    fn get_work(&self, id: &str) -> Response {
        let Some(item) = self.item(id) else {
            return unknown_work();
        };
        let sched = lock(&self.sched);
        let phase = lock(&item.phase);
        let mut body = match &*phase {
            // Not started: the agent has no lifecycle yet, and none is claimed.
            Phase::Queued => json!({
                "status": "queued",
                "lifecycle": null,
                "scheduling": {
                    "state": "queued",
                    "queue_position": sched.queue.iter().position(|q| q == id).map(|p| p + 1),
                    "queue_wait_ms": ms(item.submitted.elapsed()),
                },
            }),
            Phase::Running { admitted } => json!({
                "status": "running",
                "lifecycle": "executing",
                "scheduling": {
                    "state": "admitted",
                    "queue_wait_ms": ms(admitted.saturating_duration_since(item.submitted)),
                    "running_ms": ms(admitted.elapsed()),
                },
            }),
            Phase::Finished(f) => {
                let m = &f.result["measurement"];
                json!({
                    "status": f.state,
                    "lifecycle": f.ran.then_some(f.state),
                    "result": f.result,
                    "scheduling": {
                        "state": "finished",
                        "queue_wait_ms": ms(f.queue_wait),
                        "time_to_first_model_call_ms": f.first_model_call.map(ms),
                        "work_duration_ms": f.work_duration.map(ms),
                    },
                    // Queue wait is the scheduler's; the rest are the runtime's own measurements.
                    "timing": {
                        "queue_wait_ms": ms(f.queue_wait),
                        "model_ms": m["model_latency_ms"],
                        "execution_ms": m["compute_latency_ms"],
                        "local_decision_ms": m["local_decision_latency_ms"],
                        "runtime_total_ms": m["total_latency_ms"],
                        "work_duration_ms": f.work_duration.map(ms),
                        "turns": m["turns"],
                    },
                })
            }
        };
        body["work_id"] = json!(id);
        body["environment_id"] = json!(lock(&item.environment).clone());
        body["goal"] = json!(item.goal);
        body["kind"] = json!(item.kind.name());
        body["cancellation_requested"] = json!(item.control.cancel.load(Ordering::SeqCst));
        Response::ok(200, body)
    }

    fn get_events(&self, id: &str) -> Response {
        let Some(item) = self.item(id) else {
            return unknown_work();
        };
        let phase = lock(&item.phase);
        let (complete, events) = match &*phase {
            // The loop returns its trajectory when it ends. Nothing is invented in the meantime.
            Phase::Queued | Phase::Running { .. } => (false, Vec::new()),
            Phase::Finished(f) => (true, f.events.clone()),
        };
        // Order is guaranteed within this work only; there is no global order across works.
        Response::ok(
            200,
            json!({"work_id": id, "complete": complete, "events": events}),
        )
    }

    fn cancel(&self, id: &str) -> Response {
        let Some(item) = self.item(id) else {
            return unknown_work();
        };
        let mut sched = lock(&self.sched);
        let mut phase = lock(&item.phase);
        match &*phase {
            Phase::Finished(f) => Response::error(
                409,
                "work_already_finished",
                &format!("The work already ended: {}", f.state),
            ),
            // Never admitted, so nothing ran and this is a fact the scheduler establishes itself.
            Phase::Queued => {
                sched.queue.retain(|q| q != id);
                *phase = Phase::Finished(Box::new(Finished {
                    state: "cancelled",
                    ran: false,
                    result: json!({
                        "outcome_reason": "cancelled while queued; the work never started",
                    }),
                    events: Vec::new(),
                    queue_wait: item.submitted.elapsed(),
                    first_model_call: None,
                    work_duration: None,
                }));
                Response::ok(
                    200,
                    json!({
                        "work_id": id,
                        "status": "cancelled",
                        "note": "removed from the queue before it started: no model call, execution or observation occurred",
                    }),
                )
            }
            // Advisory: it stops the next model call. Nothing in flight is interrupted, the runtime
            // may still complete the work, and the outcome is whatever the runtime then reports.
            Phase::Running { .. } => {
                item.control.cancel.store(true, Ordering::SeqCst);
                Response::ok(
                    200,
                    json!({
                        "work_id": id,
                        "status": "cancellation_requested",
                        "note": "advisory: no further model call will be made; work already in flight is not interrupted, and the outcome is whatever the runtime establishes",
                    }),
                )
            }
        }
    }

    /// Service-level counts, derived on demand from the registry so they cannot drift from it.
    /// Per-work agent metrics stay on the work (`result.measurement`, `result.context`).
    fn metrics(&self) -> Response {
        let items: Vec<Arc<Item>> = lock(&self.items).values().cloned().collect();
        let sched = lock(&self.sched);
        let (mut queued, mut started, mut active) = (0usize, 0usize, 0usize);
        let mut by_state: HashMap<&'static str, usize> = HashMap::new();
        let (mut wait, mut duration, mut model, mut execution) = (
            Stat::default(),
            Stat::default(),
            Stat::default(),
            Stat::default(),
        );
        for item in &items {
            match &*lock(&item.phase) {
                Phase::Queued => queued += 1,
                Phase::Running { admitted } => {
                    started += 1;
                    active += 1;
                    wait.add(
                        admitted
                            .saturating_duration_since(item.submitted)
                            .as_secs_f64()
                            * 1e3,
                    );
                }
                Phase::Finished(f) => {
                    *by_state.entry(f.state).or_default() += 1;
                    if f.ran {
                        started += 1;
                        wait.add(f.queue_wait.as_secs_f64() * 1e3);
                        if let Some(d) = f.work_duration {
                            duration.add(d.as_secs_f64() * 1e3);
                        }
                        let m = &f.result["measurement"];
                        if let Some(v) = m["model_latency_ms"].as_f64() {
                            model.add(v);
                        }
                        if let Some(v) = m["compute_latency_ms"].as_f64() {
                            execution.add(v);
                        }
                    }
                }
            }
        }
        let count = |k: &str| by_state.get(k).copied().unwrap_or(0);
        Response::ok(
            200,
            json!({
                "submitted_work": items.len(),
                "rejected_work": self.rejected.load(Ordering::SeqCst),
                "queued_work": queued,
                "started_work": started,
                "active_work": active,
                "completed_work": count("completed"),
                "blocked_work": count("blocked"),
                "failed_work": count("failed"),
                "escalated_work": count("escalated"),
                "limit_reached_work": count("limit_reached"),
                "cancelled_work": count("cancelled"),
                "max_concurrent_work": self.capacity.max_concurrent,
                "max_queued_work": self.capacity.max_queued,
                "queue_length": sched.queue.len(),
                "queue_wait_ms": wait.json(),
                "work_duration_ms": duration.json(),
                "model_ms": model.json(),
                "execution_ms": execution.json(),
            }),
        )
    }
}

#[derive(Default)]
struct Stat {
    count: usize,
    total: f64,
    max: f64,
}

impl Stat {
    fn add(&mut self, v: f64) {
        self.count += 1;
        self.total += v;
        self.max = self.max.max(v);
    }

    fn json(&self) -> Value {
        let r = |v: f64| (v * 1000.0).round() / 1000.0;
        json!({"count": self.count, "total": r(self.total), "max": r(self.max)})
    }
}

fn unknown_work() -> Response {
    Response::error(404, "work_not_found", "Unknown work id")
}

/// A goal, and optionally its kind, from a body that is `{"goal": "<text>"}` or
/// `{"goal": "<text>", "kind": "change" | "verify" | "inspect"}`. Anything else is refused: a field
/// naming an id, receipt, observation, executable or configuration is an attempt to supply
/// authority, and it is rejected rather than ignored. The kind says how Chip will judge completion;
/// it grants nothing, and the default is `change`.
fn parse_goal(body: &[u8]) -> Result<(String, GoalKind), Response> {
    let bad = |code: &str, message: &str| Err(Response::error(400, code, message));
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(body) else {
        return bad("malformed_request", "The body must be a JSON object");
    };
    if let Some(name) = fields
        .keys()
        .find(|k| !matches!(k.as_str(), "goal" | "kind"))
    {
        let shown: String = name.chars().filter(|c| !c.is_control()).take(40).collect();
        return bad(
            "unknown_field",
            &format!("Only `goal` and `kind` are accepted; `{shown}` is a runtime concern"),
        );
    }
    let kind = match fields.get("kind") {
        None => GoalKind::Change,
        Some(Value::String(k)) => match GoalKind::parse(k) {
            Some(kind) => kind,
            None => return bad("invalid_kind", "The kind must be change, verify or inspect"),
        },
        Some(_) => return bad("invalid_kind", "The kind must be change, verify or inspect"),
    };
    match fields.get("goal") {
        None => bad("missing_goal", "A goal is required"),
        Some(Value::String(goal)) if goal.trim().is_empty() => {
            bad("empty_goal", "The goal must not be empty")
        }
        Some(Value::String(goal)) if goal_is_acceptable(goal) => {
            Ok((goal.trim().to_string(), kind))
        }
        Some(Value::String(_)) => bad(
            "invalid_goal",
            "The goal must be plain text of at most 2000 bytes",
        ),
        Some(_) => bad("malformed_request", "The goal must be a string"),
    }
}

// ---- projecting the runtime's report ---------------------------------------------------------------

fn finish(work: &SoftwareWork, environment: &EnvironmentDescription) -> Finished {
    let result =
        serde_json::from_str(&render_json(work, environment)).expect("the work report is JSON");
    Finished {
        state: work.report.outcome.terminal_state().name(),
        ran: true,
        result,
        events: work.report.events.iter().map(event_json).collect(),
        queue_wait: Duration::ZERO,
        first_model_call: None,
        work_duration: None,
    }
}

/// One recorded event, as the runtime recorded it. Every event is a runtime fact: model output is
/// not an event here, and no field of a model reply is carried.
fn event_json(event: &WorkEvent) -> Value {
    match event {
        WorkEvent::WorkStarted { goal, limits, .. } => json!({
            "kind": "WorkStarted", "goal": goal,
            "max_turns": limits.max_turns, "max_executions": limits.max_executions,
        }),
        WorkEvent::Capability(e) => match e {
            CapabilityEvent::CapabilitiesRequested => json!({"kind": "CapabilitiesRequested"}),
            CapabilityEvent::CapabilitiesAvailable { count } => {
                json!({"kind": "CapabilitiesAvailable", "count": count})
            }
            CapabilityEvent::CapabilitiesUnavailable { reason } => {
                json!({"kind": "CapabilitiesUnavailable", "reason": reason})
            }
        },
        WorkEvent::DecisionStarted { turn, .. } => {
            json!({"kind": "DecisionStarted", "turn": turn + 1})
        }
        WorkEvent::LocalDecision { turn, decision, .. } => {
            json!({"kind": "LocalDecision", "turn": turn + 1, "decision": decision})
        }
        WorkEvent::ModelEscalation {
            turn,
            reason,
            context,
            ..
        } => json!({
            "kind": "ModelEscalation", "turn": turn + 1, "reason": reason,
            "context_bytes": context.bytes,
        }),
        WorkEvent::ModelCalled {
            turn,
            usage,
            succeeded,
            ..
        } => json!({
            "kind": "ModelCalled", "turn": turn + 1, "succeeded": succeeded,
            "prompt_tokens": usage.map(|u| u.prompt_tokens),
            "completion_tokens": usage.map(|u| u.completion_tokens),
        }),
        WorkEvent::ContextLimit {
            turn,
            request_bytes,
            budget_bytes,
            ..
        } => json!({
            "kind": "ContextLimit", "turn": turn + 1,
            "request_bytes": request_bytes, "budget_bytes": budget_bytes,
        }),
        WorkEvent::DecisionMade { turn, decision, .. } => {
            json!({"kind": "DecisionMade", "turn": turn + 1, "decision": decision})
        }
        WorkEvent::CapabilityRequested {
            turn, capability, ..
        } => {
            json!({"kind": "CapabilityRequested", "turn": turn + 1, "capability": capability.to_string()})
        }
        WorkEvent::Execution(e) => match e {
            ExecutionEvent::ExecutionRequested { id, .. } => {
                json!({"kind": "ExecutionRequested", "execution_id": id.to_string()})
            }
            ExecutionEvent::ExecutionStarted { id } => {
                json!({"kind": "ExecutionStarted", "execution_id": id.to_string()})
            }
            ExecutionEvent::ExecutionCompleted { id, output } => json!({
                "kind": "ExecutionCompleted", "execution_id": id.to_string(),
                "output_bytes": output.len(),
            }),
            ExecutionEvent::ExecutionFailed { id, reason } => json!({
                "kind": "ExecutionFailed", "execution_id": id.to_string(), "reason": reason,
            }),
        },
        WorkEvent::ObservationRecorded {
            turn,
            execution_id,
            kind,
            receipt_id,
            ..
        } => json!({
            "kind": "ObservationRecorded", "turn": turn + 1,
            "execution_id": execution_id.to_string(),
            "observation": kind.as_str(), "receipt_id": receipt_id,
        }),
        WorkEvent::EvidenceRecorded {
            turn, capability, ..
        } => {
            json!({"kind": "EvidenceRecorded", "turn": turn + 1, "capability": capability.to_string()})
        }
        WorkEvent::EvidenceReused {
            turn,
            capability,
            receipt_id,
            ..
        } => json!({
            "kind": "EvidenceReused", "turn": turn + 1,
            "capability": capability.to_string(), "receipt_id": receipt_id,
        }),
        WorkEvent::GoalEvaluated {
            turn,
            satisfied,
            remaining,
            ..
        } => json!({
            "kind": "GoalEvaluated", "turn": turn + 1,
            "satisfied": satisfied, "remaining": remaining,
        }),
        WorkEvent::FrontierOpened {
            turn,
            item,
            kind,
            question,
            ..
        } => json!({
            "kind": "FrontierOpened", "turn": turn + 1,
            "item": item.to_string(), "frontier_kind": kind.name(), "question": question,
        }),
        WorkEvent::FrontierResolved {
            turn,
            item,
            evidence,
            ..
        } => json!({
            "kind": "FrontierResolved", "turn": turn + 1,
            "item": item.to_string(), "evidence": evidence.to_string(),
        }),
        WorkEvent::FrontierInvalidated {
            turn,
            item,
            evidence,
            ..
        } => json!({
            "kind": "FrontierInvalidated", "turn": turn + 1,
            "item": item.to_string(), "evidence": evidence.to_string(),
        }),
        WorkEvent::FrontierProgress {
            turn,
            evidence,
            resolved,
            ..
        } => json!({
            "kind": "FrontierProgress", "turn": turn + 1,
            "evidence": evidence.to_string(), "resolved": resolved,
        }),
        WorkEvent::WorkCompleted { .. } => json!({"kind": "WorkCompleted"}),
        WorkEvent::WorkEscalated { reason, .. } => {
            json!({"kind": "WorkEscalated", "reason": reason})
        }
        WorkEvent::WorkBlocked { reason, .. } => json!({"kind": "WorkBlocked", "reason": reason}),
        WorkEvent::WorkLimitReached { limit, .. } => {
            json!({"kind": "WorkLimitReached", "limit": limit.name()})
        }
        WorkEvent::WorkFailed { reason, .. } => json!({"kind": "WorkFailed", "reason": reason}),
    }
}

// ---- HTTP ------------------------------------------------------------------------------------------

struct Request {
    method: String,
    path: String,
    host: Option<String>,
    content_type: Option<String>,
    body: Vec<u8>,
}

struct Response {
    status: u16,
    body: Value,
}

impl Response {
    fn ok(status: u16, body: Value) -> Self {
        Self { status, body }
    }

    fn error(status: u16, code: &str, message: &str) -> Self {
        Self::ok(status, json!({"error": {"code": code, "message": message}}))
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        _ => "Internal Server Error",
    }
}

fn host_is_loopback(host: Option<&str>) -> bool {
    let Some(host) = host else { return false };
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or_default(),
        None => host.split(':').next().unwrap_or_default(),
    };
    matches!(name, "127.0.0.1" | "localhost" | "::1")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Reads one request: a head of bounded size, then exactly `Content-Length` bytes of a bounded
/// body. Chunked bodies are refused rather than interpreted.
async fn read_request(stream: &mut TcpStream) -> Result<Request, Response> {
    let bad = |code: &str, message: &str| Response::error(400, code, message);
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err(bad("malformed_request", "The request head is too large"));
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return Err(bad("malformed_request", "The request was incomplete")),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = std::str::from_utf8(&buf[..head_end])
        .map_err(|_| bad("malformed_request", "The request head is not text"))?;
    let mut lines = head.split("\r\n");
    let mut start = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(path), Some(version), None) =
        (start.next(), start.next(), start.next(), start.next())
    else {
        return Err(bad("malformed_request", "Malformed request line"));
    };
    if !version.starts_with("HTTP/1.") || !path.starts_with('/') {
        return Err(bad("malformed_request", "Malformed request line"));
    }
    let (mut host, mut content_type, mut length) = (None, None, None);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(bad("malformed_request", "Malformed header"));
        };
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "host" => host = Some(value.to_string()),
            "content-type" => content_type = Some(value.to_string()),
            "transfer-encoding" => {
                return Err(bad(
                    "malformed_request",
                    "Transfer-Encoding is not supported",
                ));
            }
            "content-length" => {
                let Ok(n) = value.parse::<usize>() else {
                    return Err(bad("malformed_request", "Malformed Content-Length"));
                };
                if length.replace(n).is_some() {
                    return Err(bad("malformed_request", "Duplicate Content-Length"));
                }
            }
            _ => {}
        }
    }
    let length = length.unwrap_or(0);
    if length > MAX_BODY_BYTES {
        return Err(Response::error(
            413,
            "body_too_large",
            "The request body is too large",
        ));
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < length {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return Err(bad("malformed_request", "The body was incomplete")),
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    if body.len() > length {
        return Err(bad(
            "malformed_request",
            "The body is longer than Content-Length",
        ));
    }
    Ok(Request {
        method: method.to_string(),
        path: path.to_string(),
        host,
        content_type,
        body,
    })
}

async fn connection(service: Arc<Service>, mut stream: TcpStream) {
    let response = match tokio::time::timeout(REQUEST_TIMEOUT, read_request(&mut stream)).await {
        Err(_) => Response::error(
            408,
            "request_timeout",
            "The request was not received in time",
        ),
        Ok(Err(response)) => response,
        Ok(Ok(request)) => service.handle(request).await,
    };
    let body = response.body.to_string();
    // No CORS headers, ever: a browser page may not read these responses.
    let head = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n",
        response.status,
        reason(response.status),
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Answers connections until the process ends.
pub async fn run(listener: TcpListener, service: Arc<Service>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(connection(service.clone(), stream));
    }
}

// ---- the command -----------------------------------------------------------------------------------

fn usage() -> i32 {
    eprintln!(
        "usage: chip serve [--host ADDR] [--port PORT] [--max-concurrent-work N] [--max-queued-work N]"
    );
    eprintln!(
        "       defaults: {DEFAULT_HOST}:{DEFAULT_PORT}, {DEFAULT_MAX_CONCURRENT_WORK} work at once (1 to {MAX_CONCURRENT_CEILING}), {DEFAULT_MAX_QUEUED_WORK} queued (0 to {MAX_QUEUED_CEILING}). The model comes from CHIP_PROVIDER / CHIP_MODEL / CHIP_ENDPOINT, as for `work`; works on the project in the current directory"
    );
    EXIT_USAGE
}

/// What the command line chose. `max_concurrent` is `None` until the environment says how much
/// isolation there is to spend.
#[derive(Debug, PartialEq, Eq)]
struct ServeArgs {
    addr: SocketAddr,
    max_concurrent: Option<usize>,
    max_queued: usize,
}

fn parse_args(args: &[String]) -> Result<ServeArgs, i32> {
    let (mut host, mut port) = (DEFAULT_HOST.to_string(), DEFAULT_PORT);
    let (mut max_concurrent, mut max_queued) = (None, DEFAULT_MAX_QUEUED_WORK);
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        i += 1;
        let Some(given) = args.get(i).filter(|v| !v.starts_with("--")) else {
            eprintln!("error: {flag} needs a value");
            return Err(usage());
        };
        match flag {
            "--host" => host = given.clone(),
            "--port" => match given.parse::<u16>() {
                Ok(p) => port = p,
                Err(_) => {
                    eprintln!("error: --port needs a number from 0 to 65535");
                    return Err(usage());
                }
            },
            "--max-concurrent-work" => match given.parse::<usize>() {
                Ok(n) if (1..=MAX_CONCURRENT_CEILING).contains(&n) => max_concurrent = Some(n),
                _ => {
                    eprintln!(
                        "error: --max-concurrent-work needs a number from 1 to {MAX_CONCURRENT_CEILING}"
                    );
                    return Err(usage());
                }
            },
            "--max-queued-work" => match given.parse::<usize>() {
                Ok(n) if n <= MAX_QUEUED_CEILING => max_queued = n,
                _ => {
                    eprintln!(
                        "error: --max-queued-work needs a number from 0 to {MAX_QUEUED_CEILING}"
                    );
                    return Err(usage());
                }
            },
            _ => {
                eprintln!("error: unexpected argument `{flag}`");
                return Err(usage());
            }
        }
        i += 1;
    }
    let ip: IpAddr = if host == "localhost" {
        IpAddr::from([127, 0, 0, 1])
    } else {
        host.parse().map_err(|_| {
            eprintln!("error: --host needs an IP address");
            usage()
        })?
    };
    Ok(ServeArgs {
        addr: SocketAddr::new(ip, port),
        max_concurrent,
        max_queued,
    })
}

/// `chip serve`: the runtime service over the local machine.
pub async fn serve(args: &[String]) -> i32 {
    serve_in(args, None).await
}

/// The runtime service over the given environments, or over the local machine when `None`. This
/// is how a program that embeds Rust Chip supplies its own environment provider: the service, the
/// scheduler and the work loop are the same ones `chip serve` runs.
pub async fn serve_in(args: &[String], environments: Option<Arc<Environments>>) -> i32 {
    let ServeArgs {
        addr,
        max_concurrent,
        max_queued,
    } = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let context_budget = match crate::software_work::context_budget_from_env() {
        Ok(budget) => budget,
        Err(code) => return code,
    };
    let root = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("error: no current directory ({e})");
            return EXIT_UNAVAILABLE;
        }
    };
    let selection = crate::provider_selection::Selection::default();
    let runtime = match WorkRuntime::prepare(&selection, context_budget) {
        Ok(runtime) => Arc::new(runtime),
        Err(why) => {
            eprintln!("error: {why}");
            return EXIT_UNAVAILABLE;
        }
    };
    let environments = match environments {
        Some(environments) => environments,
        None => {
            // The environment is the local machine: this project directory, which is mutable.
            let observe =
                match crate::local_environment::project_observe_from_env(|n| std::env::var(n).ok())
                {
                    Ok(observe) => observe,
                    Err(why) => {
                        eprintln!("error: {why}");
                        return EXIT_UNAVAILABLE;
                    }
                };
            match LocalEnvironmentProvider::prepare(&root).await {
                Ok(provider) => Arc::new(Environments::new(Arc::new(
                    provider.with_project_observe(observe),
                ))),
                Err(why) => {
                    eprintln!("error: {why}");
                    return EXIT_UNAVAILABLE;
                }
            }
        }
    };
    // Conservative by default, and never more than the environment can isolate.
    let isolated = environments.isolation_capacity();
    if let Some(n) = max_concurrent.filter(|n| *n > isolated) {
        eprintln!(
            "error: --max-concurrent-work {n} needs {n} isolated environments, but the local environment is one mutable project directory ({isolated}); concurrent work requires isolated environments. Use --max-concurrent-work 1"
        );
        return EXIT_USAGE;
    }
    let capacity = Capacity {
        max_concurrent: max_concurrent.unwrap_or(DEFAULT_MAX_CONCURRENT_WORK.min(isolated)),
        max_queued,
    };
    let service = match Service::new(runtime, environments, addr.ip().is_loopback(), capacity) {
        Ok(service) => service,
        Err(why) => {
            eprintln!("error: {why}");
            return EXIT_USAGE;
        }
    };
    let listener = match TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("error: cannot listen on {addr} ({e})");
            return EXIT_RUNTIME_FAILURE;
        }
    };
    let bound = listener.local_addr().unwrap_or(addr);
    if !bound.ip().is_loopback() {
        eprintln!(
            "warning: listening on {bound}, which is not loopback. This service has no authentication and is a local trusted-client interface only."
        );
    }
    // A closed stdout must not end the service.
    let mut out = std::io::stdout();
    let _ = writeln!(out, "Chip Runtime Service listening on http://{bound}");
    let _ = writeln!(
        out,
        "Local trusted-client interface: no authentication, no CORS. Work is held in memory for this process only."
    );
    let _ = writeln!(
        out,
        "Admission: {} work at once, {} queued (FIFO). {} isolated environment(s) available.",
        capacity.max_concurrent, capacity.max_queued, isolated
    );
    let _ = out.flush();
    run(listener, service).await;
    0
}

#[cfg(test)]
mod tests {
    //! The service over the real work loop, real project capabilities and a scripted model, driven
    //! through real TCP. No script here runs `pax.test`, so no work in this module can be verified
    //! or complete, and none claims to. Gates make concurrency observable without sleeping: a gated
    //! model call does not return until the test releases it.

    use std::io::{Read, Write};

    use chip_core::{EnvironmentId, EnvironmentProvider, WorkEnvironment};
    use chip_pax::PaxExecutor;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicUsize;

    use crate::local_environment::LocalEnvironment;
    use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};
    use tokio::sync::Semaphore;

    use super::*;

    const LIST: &str =
        r#"{"decision":"request_capability","capability":"project.list","inputs":{"path":"."}}"#;
    const BLOCK: &str = r#"{"decision":"block","reason":"scripted stop"}"#;
    /// An invented input on a capability that does not declare it.
    const FORGED: &str = r#"{"decision":"request_capability","capability":"project.list","inputs":{"path":".","executable":"/bin/sh"}}"#;
    const WRITE_A: &str = r#"{"decision":"request_capability","capability":"project.write","inputs":{"path":"alpha.txt","content":"from alpha"}}"#;
    const WRITE_B: &str = r#"{"decision":"request_capability","capability":"project.write","inputs":{"path":"bravo.txt","content":"from bravo"}}"#;

    #[derive(Default)]
    struct Seen {
        entered: HashMap<&'static str, usize>,
        calls: HashMap<&'static str, usize>,
        inflight: HashMap<&'static str, usize>,
        max_inflight: HashMap<&'static str, usize>,
        order: Vec<&'static str>,
        texts: HashMap<&'static str, Vec<String>>,
    }

    /// Replies by which marker the request carries (the goal is in every request), so concurrent
    /// works never share a script. Every response id is unique to its marker and call.
    struct Model {
        scripts: Mutex<HashMap<&'static str, VecDeque<&'static str>>>,
        gates: HashMap<&'static str, Semaphore>,
        panics: Vec<&'static str>,
        seen: Mutex<Seen>,
    }

    impl Model {
        fn new(scripts: &[(&'static str, &[&'static str])]) -> Self {
            Self {
                scripts: Mutex::new(
                    scripts
                        .iter()
                        .map(|(m, r)| (*m, r.iter().copied().collect()))
                        .collect(),
                ),
                gates: HashMap::new(),
                panics: Vec::new(),
                seen: Mutex::new(Seen::default()),
            }
        }

        fn gated(mut self, markers: &[&'static str]) -> Self {
            self.gates = markers.iter().map(|m| (*m, Semaphore::new(0))).collect();
            self
        }

        fn panicking(mut self, markers: &[&'static str]) -> Self {
            self.panics = markers.to_vec();
            self
        }

        fn arc(self) -> Arc<Self> {
            Arc::new(self)
        }

        fn entered(&self, m: &str) -> usize {
            lock(&self.seen).entered.get(m).copied().unwrap_or(0)
        }

        fn calls(&self, m: &str) -> usize {
            lock(&self.seen).calls.get(m).copied().unwrap_or(0)
        }

        fn max_inflight(&self, m: &str) -> usize {
            lock(&self.seen).max_inflight.get(m).copied().unwrap_or(0)
        }

        fn total_entered(&self) -> usize {
            lock(&self.seen).entered.values().sum()
        }

        fn order(&self) -> Vec<&'static str> {
            lock(&self.seen).order.clone()
        }

        fn texts(&self, m: &str) -> Vec<String> {
            lock(&self.seen).texts.get(m).cloned().unwrap_or_default()
        }

        fn release(&self, m: &str) {
            self.gates[m].add_permits(100);
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for Model {
        async fn complete(&self, r: ModelRequest) -> Result<ModelResponse, FxError> {
            let text: String = r.messages.iter().map(|m| m.content.as_str()).collect();
            let marker = {
                let scripts = lock(&self.scripts);
                *scripts
                    .keys()
                    .find(|m| text.contains(**m))
                    .ok_or_else(|| FxError::Provider("a request with no known marker".into()))?
            };
            {
                let mut seen = lock(&self.seen);
                *seen.entered.entry(marker).or_default() += 1;
                if !seen.order.contains(&marker) {
                    seen.order.push(marker);
                }
                let now = {
                    let n = seen.inflight.entry(marker).or_default();
                    *n += 1;
                    *n
                };
                let max = seen.max_inflight.entry(marker).or_default();
                *max = (*max).max(now);
                seen.texts.entry(marker).or_default().push(text);
            }
            let done = |this: &Self| {
                *lock(&this.seen).inflight.entry(marker).or_default() -= 1;
            };
            if self.panics.contains(&marker) {
                done(self);
                panic!("scripted panic in {marker}");
            }
            if let Some(gate) = self.gates.get(marker) {
                gate.acquire().await.unwrap().forget();
            }
            let n = {
                let mut seen = lock(&self.seen);
                let n = seen.calls.entry(marker).or_default();
                *n += 1;
                *n
            };
            let reply = lock(&self.scripts)
                .get_mut(marker)
                .and_then(|q| q.pop_front());
            done(self);
            let reply = reply.ok_or_else(|| FxError::Provider("unscripted model call".into()))?;
            Ok(ModelResponse::new(
                format!("{}-{n}", marker.to_lowercase()),
                reply,
                Usage::new(3, 2),
            ))
        }
    }

    async fn until(what: &str, mut condition: impl FnMut() -> bool) {
        for _ in 0..1000 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    fn root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chip-serve-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn one() -> u8 { 1 }\n").unwrap();
        dir
    }

    fn runtime(model: Arc<Model>) -> Arc<WorkRuntime> {
        Arc::new(WorkRuntime::for_test(model))
    }

    fn cap(max_concurrent: usize, max_queued: usize) -> Capacity {
        Capacity {
            max_concurrent,
            max_queued,
        }
    }

    /// A local environment over `dir`, as `chip work` would have, with an opaque test id.
    fn local(id: &str, dir: &Path) -> LocalEnvironment {
        LocalEnvironment::new(
            EnvironmentId::new(id),
            dir,
            PaxExecutor::new(dir),
            EnvironmentDescription::default(),
        )
    }

    /// Isolated environments for the tests: each is a real local environment over its own
    /// directory. It is a test double for any provider that can isolate work; Chip itself ships
    /// no such provider.
    struct Pool {
        roots: Vec<PathBuf>,
        free: Mutex<Vec<usize>>,
        capacity: usize,
        /// Hands the first environment to everyone: a provider that does not isolate.
        shares_first: bool,
        /// Refuses every acquisition.
        unavailable: bool,
        acquired: AtomicUsize,
        released: AtomicUsize,
        holders: Mutex<Vec<(String, String)>>,
    }

    impl Pool {
        fn isolated(tag: &str, n: usize) -> Arc<Self> {
            let roots: Vec<PathBuf> = (0..n).map(|i| root(&format!("{tag}-{i}"))).collect();
            Arc::new(Self {
                free: Mutex::new((0..n).rev().collect()),
                capacity: n,
                roots,
                shares_first: false,
                unavailable: false,
                acquired: AtomicUsize::new(0),
                released: AtomicUsize::new(0),
                holders: Mutex::new(Vec::new()),
            })
        }

        fn misbehaving(
            tag: &str,
            capacity: usize,
            shares_first: bool,
            unavailable: bool,
        ) -> Arc<Self> {
            Arc::new(Self {
                roots: vec![root(tag)],
                free: Mutex::new(Vec::new()),
                capacity,
                shares_first,
                unavailable,
                acquired: AtomicUsize::new(0),
                released: AtomicUsize::new(0),
                holders: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait::async_trait]
    impl EnvironmentProvider for Pool {
        fn isolation_capacity(&self) -> usize {
            self.capacity
        }

        async fn acquire(
            &self,
            work: &WorkId,
        ) -> Result<Arc<dyn WorkEnvironment>, EnvironmentError> {
            if self.unavailable {
                return Err(EnvironmentError::Unavailable(
                    "no environment could be provisioned".into(),
                ));
            }
            let index = if self.shares_first {
                0
            } else {
                lock(&self.free)
                    .pop()
                    .ok_or_else(|| EnvironmentError::Unavailable("pool exhausted".into()))?
            };
            self.acquired.fetch_add(1, Ordering::SeqCst);
            let id = format!("env_test_{index}");
            lock(&self.holders).push((work.to_string(), id.clone()));
            Ok(Arc::new(local(&id, &self.roots[index])))
        }

        fn release(&self, _work: &WorkId, environment: &EnvironmentId) {
            self.released.fetch_add(1, Ordering::SeqCst);
            if !self.shares_first {
                let index = environment
                    .as_str()
                    .rsplit('_')
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap();
                lock(&self.free).push(index);
            }
        }
    }

    async fn start_pool(
        model: Arc<Model>,
        pool: Arc<Pool>,
        capacity: Capacity,
    ) -> (SocketAddr, Arc<Service>, PathBuf) {
        let dir = pool.roots[0].clone();
        let environments = Arc::new(Environments::new(pool));
        let service = Service::new(runtime(model), environments, true, capacity).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(run(listener, service.clone()));
        (addr, service, dir)
    }

    async fn start_with(
        model: Arc<Model>,
        tag: &str,
        capacity: Capacity,
    ) -> (SocketAddr, Arc<Service>, PathBuf) {
        start_pool(
            model,
            Pool::isolated(tag, capacity.max_concurrent),
            capacity,
        )
        .await
    }

    async fn start(model: Arc<Model>, tag: &str) -> (SocketAddr, Arc<Service>, PathBuf) {
        start_with(model, tag, cap(4, 8)).await
    }

    struct Reply {
        status: u16,
        head: String,
        body: Value,
    }

    fn raw(addr: SocketAddr, bytes: &[u8]) -> Reply {
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        stream.write_all(bytes).unwrap();
        let mut out = Vec::new();
        stream.read_to_end(&mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        Reply {
            status: head.split(' ').nth(1).unwrap().parse().unwrap(),
            head: head.to_ascii_lowercase(),
            body: serde_json::from_str(body).unwrap_or(Value::Null),
        }
    }

    fn call(addr: SocketAddr, method: &str, path: &str, body: Option<&str>) -> Reply {
        let body = body.unwrap_or("");
        raw(
            addr,
            format!(
                "{method} {path} HTTP/1.1\r\nHost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
    }

    fn post(addr: SocketAddr, goal: &str) -> Reply {
        call(
            addr,
            "POST",
            "/v1/work",
            Some(&json!({"goal": goal}).to_string()),
        )
    }

    /// Submits and returns the id.
    fn submit(addr: SocketAddr, goal: &str) -> String {
        let r = post(addr, goal);
        assert_eq!(r.status, 202, "{}", r.body);
        r.body["work_id"].as_str().unwrap().to_string()
    }

    fn get(addr: SocketAddr, id: &str) -> Value {
        let r = call(addr, "GET", &format!("/v1/work/{id}"), None);
        assert_eq!(r.status, 200);
        r.body
    }

    fn status(addr: SocketAddr, id: &str) -> String {
        get(addr, id)["status"].as_str().unwrap().to_string()
    }

    fn metrics(addr: SocketAddr) -> Value {
        let r = call(addr, "GET", "/v1/metrics", None);
        assert_eq!(r.status, 200);
        r.body
    }

    async fn finished(addr: SocketAddr, id: &str) -> Value {
        for _ in 0..1000 {
            let body = get(addr, id);
            if body["status"] != "running" && body["status"] != "queued" {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("work did not finish");
    }

    fn events(addr: SocketAddr, id: &str) -> Vec<Value> {
        let r = call(addr, "GET", &format!("/v1/work/{id}/events"), None);
        assert_eq!(r.status, 200);
        r.body["events"].as_array().unwrap().clone()
    }

    fn kinds(events: &[Value]) -> Vec<String> {
        events
            .iter()
            .map(|e| e["kind"].as_str().unwrap().to_string())
            .collect()
    }

    fn execution_ids(events: &[Value]) -> Vec<String> {
        events
            .iter()
            .filter(|e| e["kind"] == "ExecutionStarted")
            .map(|e| e["execution_id"].as_str().unwrap().to_string())
            .collect()
    }

    // ---- the surface ----------------------------------------------------------------------------

    #[test]
    fn the_defaults_are_loopback_8765_two_at_once_and_flags_override_them() {
        let none: Vec<String> = Vec::new();
        let default = ServeArgs {
            addr: "127.0.0.1:8765".parse().unwrap(),
            max_concurrent: None,
            max_queued: 32,
        };
        assert_eq!(parse_args(&none), Ok(default));
        assert_eq!(DEFAULT_MAX_CONCURRENT_WORK, 2);
        assert_eq!(DEFAULT_MAX_QUEUED_WORK, 32);
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            parse_args(&a(&[
                "--host",
                "127.0.0.1",
                "--port",
                "9000",
                "--max-concurrent-work",
                "5",
                "--max-queued-work",
                "0"
            ])),
            Ok(ServeArgs {
                addr: "127.0.0.1:9000".parse().unwrap(),
                max_concurrent: Some(5),
                max_queued: 0
            })
        );
        for bad in [
            &["--port", "x"][..],
            &["--host", "not-an-ip"],
            &["--model", "m"],
            &["--port"],
            &["--max-concurrent-work", "0"],
            &["--max-concurrent-work", "65"],
            &["--max-concurrent-work", "many"],
            &["--max-queued-work", "-1"],
            &["--max-queued-work", "1025"],
        ] {
            assert!(parse_args(&a(bad)).is_err(), "{bad:?}");
        }
        assert!(DEFAULT_HOST.parse::<IpAddr>().unwrap().is_loopback());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn health_is_service_health_and_nothing_more() {
        let (addr, _s, _d) = start(Model::new(&[]).arc(), "health").await;
        let r = call(addr, "GET", "/health", None);
        assert_eq!(r.status, 200);
        assert_eq!(r.body, json!({"status": "ok"}));
        // No CORS, ever; and a preflight is not answered as permission.
        assert!(!r.head.contains("access-control"), "{}", r.head);
        let options = call(addr, "OPTIONS", "/v1/work", None);
        assert_eq!(options.status, 405);
        assert!(!options.head.contains("access-control"));
        assert_eq!(call(addr, "GET", "/nowhere", None).status, 404);
        assert_eq!(call(addr, "DELETE", "/health", None).status, 405);
        assert_eq!(call(addr, "POST", "/v1/metrics", None).status, 405);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_loopback_host_names_are_answered() {
        let (addr, _s, _d) = start(Model::new(&[]).arc(), "host").await;
        let r = raw(addr, b"GET /health HTTP/1.1\r\nHost: evil.example\r\n\r\n");
        assert_eq!(r.status, 403);
        assert_eq!(r.body["error"]["code"], "host_not_allowed");
        assert_eq!(raw(addr, b"GET /health HTTP/1.1\r\n\r\n").status, 403);
        let r = raw(
            addr,
            b"GET /health HTTP/1.1\r\nHost: localhost:8765\r\n\r\n",
        );
        assert_eq!(r.status, 200);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn malformed_requests_are_refused_and_start_nothing() {
        let model = Model::new(&[]).arc();
        let (addr, service, _d) = start(model.clone(), "malformed").await;
        let h = |body: &str| {
            format!(
                "POST /v1/work HTTP/1.1\r\nHost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            )
        };
        for body in [
            "",
            "not json",
            "[]",
            "\"goal\"",
            "{}",
            r#"{"goal":null}"#,
            r#"{"goal":7}"#,
            r#"{"goal":""}"#,
            r#"{"goal":"   "}"#,
            r#"{"goal":"bad\u0000control"}"#,
        ] {
            let r = raw(addr, h(body).as_bytes());
            assert_eq!(r.status, 400, "{body:?}");
            assert!(r.body["error"]["code"].is_string(), "{body:?}");
        }
        let long = json!({"goal": "x".repeat(2001)}).to_string();
        assert_eq!(raw(addr, h(&long).as_bytes()).status, 400);
        let huge = json!({"goal": "x".repeat(MAX_BODY_BYTES + 1)}).to_string();
        assert_eq!(raw(addr, h(&huge).as_bytes()).status, 413);
        let r = raw(
            addr,
            format!(
                "POST /v1/work HTTP/1.1\r\nHost: {addr}\r\ncontent-type: text/plain\r\ncontent-length: 13\r\n\r\n{{\"goal\":\"hi\"}}"
            )
            .as_bytes(),
        );
        assert_eq!(r.status, 415);
        let r = raw(
            addr,
            format!("POST /v1/work HTTP/1.1\r\nHost: {addr}\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n0\r\n\r\n")
                .as_bytes(),
        );
        assert_eq!(r.status, 400);
        assert_eq!(raw(addr, b"garbage\r\n\r\n").status, 400);
        assert!(lock(&service.items).is_empty());
        assert_eq!(model.total_entered(), 0, "no model was asked");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_cannot_supply_authority_configuration_or_a_workspace() {
        let model = Model::new(&[]).arc();
        let (addr, service, dir) = start(model.clone(), "forged").await;
        for field in [
            "work_id",
            "execution_id",
            "turn_id",
            "receipt",
            "receipt_id",
            "observation",
            "observation_id",
            "evidence",
            "executable",
            "argv",
            "cwd",
            "workspace_root",
            "root",
            "workspace",
            "worktree",
            "capability",
            "capabilities",
            "inputs",
            "provider",
            "model",
            "endpoint",
            "compute",
            "status",
            "lifecycle",
            "result",
            "events",
            "output",
            "max_turns",
            "priority",
            "queue",
            "max_concurrent_work",
        ] {
            let body = format!(r#"{{"goal":"inspect the project","{field}":"forged"}}"#);
            let r = call(addr, "POST", "/v1/work", Some(&body));
            assert_eq!(r.status, 400, "{field}");
            assert_eq!(r.body["error"]["code"], "unknown_field", "{field}");
        }
        assert!(lock(&service.items).is_empty());
        assert_eq!(model.total_entered(), 0);
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "pub fn one() -> u8 { 1 }\n"
        );
        assert_eq!(
            call(addr, "POST", "/v1/work/work_x/events", Some("{}")).status,
            405
        );
    }

    /// The kind says how Chip judges completion. It is the client's choice, like the goal; it
    /// grants nothing, defaults to `change`, and an unknown one is refused before anything runs.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_kind_of_a_goal_is_optional_validated_and_reported() {
        let answer = r#"{"decision":"complete","summary":"`one` is defined in src/lib.rs."}"#;
        let read = r#"{"decision":"request_capability","capability":"project.read","inputs":{"path":"src/lib.rs"}}"#;
        let model = Model::new(&[("ALPHA", &[LIST, read, answer]), ("BRAVO", &[BLOCK])]).arc();
        let (addr, service, _d) = start(model.clone(), "kind").await;

        for bad in [r#""deploy""#, "null", "7", r#""""#, r#""Inspect""#] {
            let body = format!(r#"{{"goal":"ALPHA where is one defined","kind":{bad}}}"#);
            let r = call(addr, "POST", "/v1/work", Some(&body));
            assert_eq!(r.status, 400, "{bad}");
            assert_eq!(r.body["error"]["code"], "invalid_kind", "{bad}");
        }
        assert!(
            lock(&service.items).is_empty(),
            "a refused request starts nothing"
        );
        assert_eq!(model.total_entered(), 0);

        let r = call(
            addr,
            "POST",
            "/v1/work",
            Some(r#"{"goal":"ALPHA where is one defined","kind":"inspect"}"#),
        );
        assert_eq!(r.status, 202, "{}", r.body);
        let id = r.body["work_id"].as_str().unwrap().to_string();
        let done = finished(addr, &id).await;
        assert_eq!(done["kind"], "inspect");
        assert_eq!(done["status"], "completed", "{done}");
        assert_eq!(done["result"]["goal_kind"], "inspect");
        // An accepted inspect answer is grounded, not verified: Chip does not interpret it.
        assert_eq!(done["result"]["verified"], false);
        assert_eq!(done["result"]["grounded"], true);
        assert_eq!(done["result"]["goal_satisfied"], true);
        assert!(
            done["result"]["answer"]
                .as_str()
                .unwrap()
                .contains("src/lib.rs")
        );
        assert_eq!(done["result"]["audit"]["clean"], true);

        // Without a kind, the work is change work, exactly as before.
        let id = submit(addr, "BRAVO inspect the project");
        let done = finished(addr, &id).await;
        assert_eq!(done["kind"], "change");
        assert_eq!(done["result"]["goal_kind"], "change");
        assert_eq!(done["result"]["answer"], Value::Null);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_work_is_404_on_every_route() {
        let (addr, _s, _d) = start(Model::new(&[]).arc(), "unknown").await;
        for (method, path) in [
            ("GET", "/v1/work/work_nope"),
            ("GET", "/v1/work/work_nope/events"),
            ("POST", "/v1/work/work_nope/cancel"),
            ("GET", "/v1/work/..%2f..%2fetc%2fpasswd"),
        ] {
            let r = call(addr, method, path, None);
            assert_eq!(r.status, 404, "{path}");
            assert_eq!(r.body["error"]["code"], "work_not_found", "{path}");
            assert_eq!(r.body["error"]["message"], "Unknown work id");
        }
    }

    // ---- one work -------------------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn post_is_prompt_the_id_is_chips_and_the_loop_runs_in_the_background() {
        let model = Model::new(&[("ALPHA", &[LIST, BLOCK])])
            .gated(&["ALPHA"])
            .arc();
        let (addr, _s, _d) = start(model.clone(), "prompt").await;
        let t0 = Instant::now();
        let r = post(addr, "ALPHA inspect the project");
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "POST waited for the work"
        );
        assert_eq!(r.status, 202);
        assert_eq!(r.body["status"], "running");
        let id = r.body["work_id"].as_str().unwrap().to_string();
        assert!(id.starts_with("work_"), "{id}");
        assert!(!id.contains("alpha"), "not a provider response id: {id}");

        until("the model call", || model.entered("ALPHA") == 1).await;
        let g = get(addr, &id);
        assert_eq!(g["status"], "running");
        assert_eq!(g["lifecycle"], "executing");
        assert_eq!(g["scheduling"]["state"], "admitted");
        assert_eq!(g["goal"], "ALPHA inspect the project");
        assert_eq!(g["cancellation_requested"], false);
        assert!(g.get("result").is_none(), "no result before there is one");
        let e = call(addr, "GET", &format!("/v1/work/{id}/events"), None);
        assert_eq!(e.body["complete"], false);
        assert_eq!(
            e.body["events"],
            json!([]),
            "no event is invented while it runs"
        );

        model.release("ALPHA");
        let done = finished(addr, &id).await;
        assert_eq!(done["status"], "blocked");
        assert_eq!(done["lifecycle"], "blocked");
        assert_eq!(done["result"]["terminal_state"], "blocked");
        assert_eq!(done["result"]["verified"], false);
        assert_eq!(done["result"]["audit"]["clean"], true);
        assert_eq!(finished(addr, &id).await, done, "terminal state is stable");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn events_are_the_runtimes_trajectory_in_order_and_match_a_direct_run() {
        let script: &[(&str, &[&str])] = &[("ALPHA", &[LIST, BLOCK])];
        let (addr, _s, dir) = start(Model::new(script).arc(), "events").await;
        let id = submit(addr, "ALPHA inspect the project");
        finished(addr, &id).await;
        let served = events(addr, &id);

        let k = kinds(&served);
        let order = [
            "WorkStarted",
            "CapabilityRequested",
            "ExecutionRequested",
            "ExecutionStarted",
            "ExecutionCompleted",
            "ObservationRecorded",
            "EvidenceRecorded",
            "WorkBlocked",
        ];
        let mut at = 0;
        for needle in order {
            at += k[at..]
                .iter()
                .position(|x| x == needle)
                .unwrap_or_else(|| panic!("{needle} missing or out of order in {k:?}"));
        }
        assert_eq!(
            k.last().unwrap(),
            "WorkBlocked",
            "nothing after the terminal event"
        );

        // The same runtime, called the way `chip work` calls it, records the same trajectory.
        let direct = runtime(Model::new(script).arc())
            .run(
                WorkId::new("direct"),
                "ALPHA inspect the project",
                GoalKind::Change,
                WorkLimits {
                    max_turns: DEFAULT_MAX_TURNS,
                    max_executions: DEFAULT_MAX_EXECUTIONS,
                },
                None,
                &local("env_direct", &dir),
            )
            .await
            .0;
        let direct: Vec<Value> = direct.report.events.iter().map(event_json).collect();
        // Identity is Chip's and derives from the work id, so the two runs differ only in that
        // (and in the context bytes that carry it); everything else is the same trajectory.
        let normalise = |events: Vec<Value>| -> Vec<Value> {
            events
                .into_iter()
                .map(|mut e| {
                    if let Some(o) = e.as_object_mut() {
                        if o.contains_key("execution_id") {
                            o.insert("execution_id".into(), Value::from("exec"));
                        }
                        // A frontier transition cites the execution it rests on, by the same identity.
                        if o.contains_key("evidence") {
                            o.insert("evidence".into(), Value::from("exec"));
                        }
                        o.remove("context_bytes");
                    }
                    e
                })
                .collect()
        };
        assert_eq!(normalise(served), normalise(direct));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rejected_request_produces_no_execution_observation_or_evidence() {
        let (addr, _s, _d) = start(Model::new(&[("ALPHA", &[FORGED])]).arc(), "rejected").await;
        let id = submit(addr, "ALPHA something");
        let done = finished(addr, &id).await;
        assert_eq!(done["status"], "blocked");
        let k = kinds(&events(addr, &id));
        for fabricated in [
            "ExecutionRequested",
            "ExecutionStarted",
            "ExecutionCompleted",
            "ObservationRecorded",
            "EvidenceRecorded",
            "EvidenceReused",
            "GoalEvaluated",
            "WorkCompleted",
        ] {
            assert!(!k.iter().any(|x| x == fabricated), "{fabricated} in {k:?}");
        }
        assert_eq!(done["result"]["measurement"]["executions"], 0);
        assert_eq!(done["result"]["measurement"]["observations"], 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn responses_carry_no_credentials_environment_or_host_paths() {
        let (addr, _s, dir) =
            start(Model::new(&[("ALPHA", &[LIST, BLOCK])]).arc(), "secrets").await;
        let id = submit(addr, "ALPHA inspect");
        finished(addr, &id).await;
        let all = format!(
            "{}{}{}",
            get(addr, &id),
            call(addr, "GET", &format!("/v1/work/{id}/events"), None).body,
            metrics(addr)
        );
        assert!(
            !all.contains(dir.to_str().unwrap()),
            "host path leaked: {all}"
        );
        for name in ["API_KEY", "TOKEN", "SECRET", "Authorization"] {
            assert!(!all.contains(name), "{name} in {all}");
        }
    }

    // ---- many works -----------------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn two_works_execute_at_the_same_time_without_waiting_for_each_other() {
        let model = Model::new(&[("ALPHA", &[LIST, BLOCK]), ("BRAVO", &[LIST, BLOCK])])
            .gated(&["ALPHA", "BRAVO"])
            .arc();
        let (addr, _s, _d) = start_with(model.clone(), "simultaneous", cap(2, 4)).await;
        let a = submit(addr, "ALPHA first");
        let b = submit(addr, "BRAVO second");
        // Both are inside a model call at once, and neither has finished.
        until("both in the model", || {
            model.entered("ALPHA") == 1 && model.entered("BRAVO") == 1
        })
        .await;
        assert_eq!(status(addr, &a), "running");
        assert_eq!(status(addr, &b), "running");
        let m = metrics(addr);
        assert_eq!(
            (m["active_work"].as_u64(), m["queued_work"].as_u64()),
            (Some(2), Some(0))
        );
        model.release("ALPHA");
        model.release("BRAVO");
        assert_eq!(finished(addr, &a).await["status"], "blocked");
        assert_eq!(finished(addr, &b).await["status"], "blocked");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn work_past_the_limit_is_queued_not_running_and_starts_only_when_capacity_opens() {
        let model = Model::new(&[
            ("ALPHA", &[LIST, BLOCK]),
            ("BRAVO", &[LIST, BLOCK]),
            ("CHARLIE", &[BLOCK]),
        ])
        .gated(&["ALPHA", "BRAVO"])
        .arc();
        let (addr, _s, _d) = start_with(model.clone(), "limit", cap(2, 4)).await;
        let a = submit(addr, "ALPHA");
        let b = submit(addr, "BRAVO");
        let queued = post(addr, "CHARLIE");
        assert_eq!(queued.status, 202);
        assert_eq!(
            queued.body["status"], "queued",
            "not running until it starts"
        );
        let c = queued.body["work_id"].as_str().unwrap().to_string();
        until("A and B to be in the model", || {
            model.entered("ALPHA") == 1 && model.entered("BRAVO") == 1
        })
        .await;
        tokio::time::sleep(Duration::from_millis(80)).await;

        let q = get(addr, &c);
        assert_eq!(q["status"], "queued");
        assert_eq!(
            q["lifecycle"],
            Value::Null,
            "the agent has no lifecycle before it starts"
        );
        assert_eq!(q["scheduling"]["state"], "queued");
        assert_eq!(q["scheduling"]["queue_position"], 1);
        assert_eq!(
            model.entered("CHARLIE"),
            0,
            "C did not start before capacity existed"
        );
        assert_eq!(events(addr, &c), Vec::<Value>::new());
        let m = metrics(addr);
        assert_eq!(
            (
                m["active_work"].as_u64(),
                m["queued_work"].as_u64(),
                m["started_work"].as_u64()
            ),
            (Some(2), Some(1), Some(2))
        );

        // A ends; C takes its place while B is still running.
        model.release("ALPHA");
        assert_eq!(finished(addr, &a).await["status"], "blocked");
        let done = finished(addr, &c).await;
        assert_eq!(done["status"], "blocked");
        assert_eq!(status(addr, &b), "running");
        assert_eq!(model.entered("CHARLIE"), 1);

        // Queue wait is the scheduler's number and is kept apart from the runtime's own timings.
        assert!(
            done["scheduling"]["queue_wait_ms"].as_f64().unwrap() >= 70.0,
            "{}",
            done["scheduling"]
        );
        let t = &done["timing"];
        assert!(
            t["model_ms"].is_number() && t["execution_ms"].is_number(),
            "{t}"
        );
        assert!(
            t["work_duration_ms"].is_number() && t["queue_wait_ms"].is_number(),
            "{t}"
        );
        assert!(done["scheduling"]["time_to_first_model_call_ms"].is_number());
        assert!(
            t["model_ms"].as_f64().unwrap() < t["queue_wait_ms"].as_f64().unwrap(),
            "queue wait is not counted as model time: {t}"
        );
        model.release("BRAVO");
        finished(addr, &b).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn admission_is_fifo() {
        let model = Model::new(&[
            ("ALPHA", &[BLOCK]),
            ("BRAVO", &[BLOCK]),
            ("CHARLIE", &[BLOCK]),
            ("DELTA", &[BLOCK]),
        ])
        .gated(&["ALPHA"])
        .arc();
        let (addr, _s, _d) = start_with(model.clone(), "fifo", cap(1, 8)).await;
        let a = submit(addr, "ALPHA");
        until("A to start", || model.entered("ALPHA") == 1).await;
        let rest: Vec<String> = ["BRAVO", "CHARLIE", "DELTA"]
            .iter()
            .map(|g| submit(addr, g))
            .collect();
        for (i, id) in rest.iter().enumerate() {
            assert_eq!(get(addr, id)["scheduling"]["queue_position"], i + 1);
        }
        model.release("ALPHA");
        for id in std::iter::once(&a).chain(&rest) {
            finished(addr, id).await;
        }
        assert_eq!(model.order(), ["ALPHA", "BRAVO", "CHARLIE", "DELTA"]);
        assert_eq!(model.max_inflight("BRAVO"), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn queued_work_cancelled_before_admission_never_runs() {
        let model = Model::new(&[
            ("ALPHA", &[BLOCK]),
            ("BRAVO", &[BLOCK]),
            ("CHARLIE", &[LIST, BLOCK]),
        ])
        .gated(&["ALPHA", "BRAVO"])
        .arc();
        let (addr, _s, _d) = start_with(model.clone(), "queuecancel", cap(2, 4)).await;
        let a = submit(addr, "ALPHA");
        let b = submit(addr, "BRAVO");
        let c = submit(addr, "CHARLIE");
        until("A and B running", || {
            model.entered("ALPHA") == 1 && model.entered("BRAVO") == 1
        })
        .await;
        assert_eq!(status(addr, &c), "queued");

        let cancelled = call(addr, "POST", &format!("/v1/work/{c}/cancel"), None);
        assert_eq!(cancelled.status, 200);
        assert_eq!(cancelled.body["status"], "cancelled");
        let g = get(addr, &c);
        assert_eq!(g["status"], "cancelled");
        assert_eq!(
            g["lifecycle"],
            Value::Null,
            "no agent lifecycle ever existed"
        );
        let e = call(addr, "GET", &format!("/v1/work/{c}/events"), None);
        assert_eq!(
            (e.body["complete"].clone(), e.body["events"].clone()),
            (json!(true), json!([]))
        );

        // Capacity opens; the cancelled work is not admitted.
        model.release("ALPHA");
        model.release("BRAVO");
        finished(addr, &a).await;
        finished(addr, &b).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(status(addr, &c), "cancelled");
        assert_eq!(model.entered("CHARLIE"), 0, "0 model requests");
        assert_eq!(model.calls("CHARLIE"), 0, "0 model calls");
        assert!(model.texts("CHARLIE").is_empty());
        assert!(
            get(addr, &c)
                .get("result")
                .is_some_and(|r| r.get("measurement").is_none())
        );

        let again = call(addr, "POST", &format!("/v1/work/{c}/cancel"), None);
        assert_eq!(again.status, 409);
        let m = metrics(addr);
        assert_eq!(m["cancelled_work"], 1);
        assert_eq!(m["started_work"], 2, "C never started");
        assert_eq!(m["submitted_work"], 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_full_queue_is_refused_explicitly_and_nothing_is_dropped() {
        let model = Model::new(&[
            ("ALPHA", &[BLOCK]),
            ("BRAVO", &[BLOCK]),
            ("CHARLIE", &[BLOCK]),
            ("DELTA", &[BLOCK]),
        ])
        .gated(&["ALPHA"])
        .arc();
        let (addr, _s, _d) = start_with(model.clone(), "backpressure", cap(1, 2)).await;
        let a = submit(addr, "ALPHA");
        until("A to start", || model.entered("ALPHA") == 1).await;
        let b = submit(addr, "BRAVO");
        let c = submit(addr, "CHARLIE");
        let full = post(addr, "DELTA");
        assert_eq!(full.status, 429);
        assert_eq!(full.body["error"]["code"], "queue_full");
        let m = metrics(addr);
        assert_eq!(
            (
                m["submitted_work"].as_u64(),
                m["rejected_work"].as_u64(),
                m["queued_work"].as_u64()
            ),
            (Some(3), Some(1), Some(2))
        );
        assert_eq!(model.entered("DELTA"), 0, "the refused work did not run");

        model.release("ALPHA");
        for id in [&a, &b, &c] {
            assert_eq!(
                finished(addr, id).await["status"],
                "blocked",
                "accepted work is never dropped"
            );
        }
        // Space again: the same request is now accepted.
        assert_eq!(post(addr, "DELTA").status, 202);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn with_no_queue_a_work_past_the_limit_is_refused() {
        let model = Model::new(&[("ALPHA", &[BLOCK]), ("BRAVO", &[BLOCK])])
            .gated(&["ALPHA"])
            .arc();
        let (addr, _s, _d) = start_with(model.clone(), "noqueue", cap(1, 0)).await;
        let a = submit(addr, "ALPHA");
        assert_eq!(post(addr, "BRAVO").status, 429);
        model.release("ALPHA");
        finished(addr, &a).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_works_share_nothing_goals_events_context_results_and_ids() {
        let model = Model::new(&[("ALPHA", &[LIST, BLOCK]), ("BRAVO", &[LIST, LIST, BLOCK])])
            .gated(&["ALPHA", "BRAVO"])
            .arc();
        let (addr, _s, _d) = start_with(model.clone(), "isolation", cap(2, 4)).await;
        let a = submit(addr, "ALPHA inspect");
        let b = submit(addr, "BRAVO survey");
        assert_ne!(a, b);
        // Overlap is real: both are in a model call before either gets an answer.
        until("both in the model", || {
            model.entered("ALPHA") == 1 && model.entered("BRAVO") == 1
        })
        .await;
        model.release("ALPHA");
        model.release("BRAVO");
        let (da, db) = (finished(addr, &a).await, finished(addr, &b).await);

        for (done, id, goal) in [(&da, &a, "ALPHA inspect"), (&db, &b, "BRAVO survey")] {
            assert_eq!(done["work_id"], id.as_str());
            assert_eq!(done["goal"], goal);
            assert_eq!(done["result"]["goal"], goal);
        }
        assert_eq!(da["result"]["measurement"]["executions"], 1);
        assert_eq!(db["result"]["measurement"]["executions"], 2);
        assert_eq!(da["result"]["measurement"]["observations"], 1);
        assert_eq!(db["result"]["measurement"]["observations"], 2);
        assert_ne!(da["result"]["measurement"], db["result"]["measurement"]);
        assert_ne!(da["result"]["context"], db["result"]["context"]);

        // Events: each stream is its own, starts with its own goal, and shares no execution id.
        let (ea, eb) = (events(addr, &a), events(addr, &b));
        assert!(ea[0]["goal"].as_str().unwrap().contains("ALPHA"));
        assert!(eb[0]["goal"].as_str().unwrap().contains("BRAVO"));
        assert!(!ea[0]["goal"].as_str().unwrap().contains("BRAVO"));
        assert!(!eb[0]["goal"].as_str().unwrap().contains("ALPHA"));
        let (xa, xb) = (execution_ids(&ea), execution_ids(&eb));
        assert_eq!((xa.len(), xb.len()), (1, 2));
        // Execution ids are Chip's: derived from each work's own id, so they cannot collide.
        assert!(
            xa.iter().all(|id| id.starts_with(&format!("{a}-exec-"))),
            "{xa:?}"
        );
        assert!(
            xb.iter().all(|id| id.starts_with(&format!("{b}-exec-"))),
            "{xb:?}"
        );
        assert!(!ea.iter().any(|e| e.to_string().contains("bravo")));
        assert!(!eb.iter().any(|e| e.to_string().contains("alpha")));
        assert_eq!(
            kinds(&ea)
                .iter()
                .filter(|k| *k == "ObservationRecorded")
                .count(),
            1
        );
        assert_eq!(
            kinds(&eb)
                .iter()
                .filter(|k| *k == "ObservationRecorded")
                .count(),
            2
        );

        // What each model was shown: never the other's goal, execution ids or observations.
        assert!(model.texts("ALPHA").iter().all(|t| !t.contains("BRAVO")));
        assert!(model.texts("BRAVO").iter().all(|t| !t.contains("ALPHA")));
        assert!(model.texts("ALPHA").len() == 2 && model.texts("BRAVO").len() == 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn each_trajectory_stays_sequential_while_works_overlap() {
        let model = Model::new(&[
            ("ALPHA", &[LIST, LIST, LIST, BLOCK]),
            ("BRAVO", &[LIST, LIST, LIST, BLOCK]),
        ])
        .gated(&["ALPHA", "BRAVO"])
        .arc();
        let (addr, _s, _d) = start_with(model.clone(), "sequential", cap(2, 4)).await;
        let a = submit(addr, "ALPHA");
        let b = submit(addr, "BRAVO");
        until("both in the model", || {
            model.entered("ALPHA") == 1 && model.entered("BRAVO") == 1
        })
        .await;
        model.release("ALPHA");
        model.release("BRAVO");
        finished(addr, &a).await;
        finished(addr, &b).await;
        for (marker, id) in [("ALPHA", &a), ("BRAVO", &b)] {
            assert_eq!(
                model.max_inflight(marker),
                1,
                "one model call at a time for {marker}"
            );
            assert_eq!(model.calls(marker), 4);
            // Within the work: an execution is closed before the next one opens, and no model
            // request is made while one is open; observation follows execution.
            let mut open = false;
            let mut executions = 0;
            let mut last = String::new();
            for e in events(addr, id) {
                let kind = e["kind"].as_str().unwrap().to_string();
                match kind.as_str() {
                    "ExecutionStarted" => {
                        assert!(!open, "two simultaneous executions in {marker}");
                        open = true;
                        executions += 1;
                    }
                    "ExecutionCompleted" | "ExecutionFailed" => {
                        assert!(open);
                        open = false;
                    }
                    "ModelEscalation" | "ModelCalled" | "DecisionMade" => {
                        assert!(!open, "{kind} while an execution is open in {marker}");
                    }
                    "ObservationRecorded" => assert_eq!(last, "ExecutionCompleted"),
                    _ => {}
                }
                last = kind;
            }
            assert_eq!(executions, 3);
            assert!(!open);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_panic_in_one_work_fails_that_work_and_nothing_else() {
        let model = Model::new(&[
            ("ALPHA", &[BLOCK]),
            ("BRAVO", &[LIST, BLOCK]),
            ("CHARLIE", &[BLOCK]),
        ])
        .gated(&["BRAVO"])
        .panicking(&["ALPHA"])
        .arc();
        let (addr, _s, _d) = start_with(model.clone(), "panic", cap(2, 4)).await;
        let b = submit(addr, "BRAVO");
        let a = submit(addr, "ALPHA");
        let c = submit(addr, "CHARLIE");
        until("B in the model", || model.entered("BRAVO") == 1).await;

        let failed = finished(addr, &a).await;
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["lifecycle"], "failed");
        assert!(
            failed["result"]["outcome_reason"]
                .as_str()
                .unwrap()
                .contains("abnormally")
        );
        assert_ne!(failed["status"], "completed");
        assert_eq!(
            events(addr, &a),
            Vec::<Value>::new(),
            "no events are fabricated"
        );
        // The slot A held was freed: queued C ran, while B was still in flight.
        assert_eq!(finished(addr, &c).await["status"], "blocked");
        assert_eq!(status(addr, &b), "running");
        assert_eq!(call(addr, "GET", "/health", None).status, 200);

        model.release("BRAVO");
        let done = finished(addr, &b).await;
        assert_eq!(done["status"], "blocked");
        assert_eq!(done["result"]["measurement"]["executions"], 1);
        assert_eq!(done["result"]["audit"]["clean"], true);
        let m = metrics(addr);
        assert_eq!(
            (m["failed_work"].as_u64(), m["blocked_work"].as_u64()),
            (Some(1), Some(2))
        );
        assert_eq!(m["active_work"], 0);
    }

    // ---- the environment boundary ---------------------------------------------------------------

    fn root_of(pool: &Pool, environment_id: &str) -> PathBuf {
        let index: usize = environment_id.rsplit('_').next().unwrap().parse().unwrap();
        pool.roots[index].clone()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_works_operate_in_separate_environments_and_nothing_crosses() {
        let model = Model::new(&[("ALPHA", &[WRITE_A, BLOCK]), ("BRAVO", &[WRITE_B, BLOCK])])
            .gated(&["ALPHA", "BRAVO"])
            .arc();
        let pool = Pool::isolated("isolated-envs", 2);
        let (addr, _s, _d) = start_pool(model.clone(), pool.clone(), cap(2, 4)).await;
        let a = submit(addr, "ALPHA");
        let b = submit(addr, "BRAVO");
        until("both in the model", || {
            model.entered("ALPHA") == 1 && model.entered("BRAVO") == 1
        })
        .await;
        // Each already has its own environment while running.
        let (ga, gb) = (get(addr, &a), get(addr, &b));
        let (ea, eb) = (
            ga["environment_id"].as_str().unwrap().to_string(),
            gb["environment_id"].as_str().unwrap().to_string(),
        );
        assert_ne!(ea, eb);
        model.release("ALPHA");
        model.release("BRAVO");
        let (da, db) = (finished(addr, &a).await, finished(addr, &b).await);
        assert_eq!(
            (da["environment_id"].as_str(), db["environment_id"].as_str()),
            (Some(ea.as_str()), Some(eb.as_str()))
        );

        // Writes landed only in the writer's own environment.
        let (ra, rb) = (root_of(&pool, &ea), root_of(&pool, &eb));
        assert_ne!(ra, rb);
        assert_eq!(
            std::fs::read_to_string(ra.join("alpha.txt")).unwrap(),
            "from alpha"
        );
        assert_eq!(
            std::fs::read_to_string(rb.join("bravo.txt")).unwrap(),
            "from bravo"
        );
        assert!(!ra.join("bravo.txt").exists() && !rb.join("alpha.txt").exists());

        // Executions, observations and evidence are each work's own.
        for (done, id, own) in [(&da, &a, "alpha"), (&db, &b, "bravo")] {
            assert_eq!(done["result"]["measurement"]["executions"], 1);
            assert_eq!(done["result"]["measurement"]["observations"], 1);
            let ev = events(addr, id);
            assert_eq!(execution_ids(&ev).len(), 1);
            assert!(
                execution_ids(&ev)[0].starts_with(&format!("{id}-exec-")),
                "{own}: {ev:?}"
            );
            assert_eq!(
                kinds(&ev)
                    .iter()
                    .filter(|k| *k == "EvidenceRecorded")
                    .count(),
                1
            );
        }

        // Nothing the model was shown, and nothing the service says, names a host path; the
        // environment identity is opaque.
        for (marker, root) in [("ALPHA", &ra), ("BRAVO", &rb)] {
            let shown = model.texts(marker).join("\n");
            for r in [&ra, &rb] {
                assert!(
                    !shown.contains(r.to_str().unwrap()),
                    "{marker} was shown a host path"
                );
            }
            assert!(
                !shown.contains("env_test"),
                "{marker} was shown an environment id"
            );
            let _ = root;
        }
        let all = format!("{}{}", da, db);
        assert!(!all.contains(ra.to_str().unwrap()) && !all.contains(rb.to_str().unwrap()));
        assert!(ea.starts_with("env_test_") && !ea.contains('/'));
        assert_eq!(pool.acquired.load(Ordering::SeqCst), 2);
        assert_eq!(pool.released.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_provider_that_shares_one_environment_cannot_make_two_works_collide() {
        let model = Model::new(&[("ALPHA", &[WRITE_A, BLOCK]), ("BRAVO", &[WRITE_B, BLOCK])])
            .gated(&["ALPHA"])
            .arc();
        let pool = Pool::misbehaving("shared", 4, true, false);
        let (addr, _s, dir) = start_pool(model.clone(), pool.clone(), cap(2, 4)).await;
        let a = submit(addr, "ALPHA");
        until("A in the model", || model.entered("ALPHA") == 1).await;
        let b = submit(addr, "BRAVO");

        // B was handed A's environment; the boundary refuses it before anything happens.
        let refused = finished(addr, &b).await;
        assert_eq!(refused["status"], "failed");
        assert_eq!(refused["lifecycle"], Value::Null, "the agent never started");
        assert_eq!(refused["environment_id"], Value::Null);
        let reason = refused["result"]["outcome_reason"].as_str().unwrap();
        assert!(reason.contains("never shared"), "{reason}");
        assert_eq!(model.entered("BRAVO"), 0, "no model call");
        assert_eq!(events(addr, &b), Vec::<Value>::new());
        assert!(!dir.join("bravo.txt").exists(), "nothing was executed");
        assert_eq!(
            get(addr, &a)["status"],
            "running",
            "the owner was not disturbed"
        );

        model.release("ALPHA");
        assert_eq!(finished(addr, &a).await["status"], "blocked");
        assert_eq!(
            std::fs::read_to_string(dir.join("alpha.txt")).unwrap(),
            "from alpha"
        );
        // Only the accepted ownership was ever released.
        assert_eq!(pool.released.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_environment_is_reused_by_a_later_work_once_it_is_released() {
        let model = Model::new(&[("ALPHA", &[WRITE_A, BLOCK]), ("BRAVO", &[WRITE_B, BLOCK])]).arc();
        let pool = Pool::misbehaving("sequential", 1, true, false);
        let (addr, _s, dir) = start_pool(model.clone(), pool.clone(), cap(1, 4)).await;
        let a = submit(addr, "ALPHA");
        let da = finished(addr, &a).await;
        let b = submit(addr, "BRAVO");
        let db = finished(addr, &b).await;
        assert_eq!(da["environment_id"], "env_test_0");
        assert_eq!(
            db["environment_id"], "env_test_0",
            "the same environment, one owner at a time"
        );
        assert_eq!(
            (da["status"].as_str(), db["status"].as_str()),
            (Some("blocked"), Some("blocked"))
        );
        assert!(dir.join("alpha.txt").exists() && dir.join("bravo.txt").exists());
        assert_eq!(pool.released.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn work_with_no_environment_fails_without_a_model_call_or_execution() {
        let model = Model::new(&[("ALPHA", &[WRITE_A, BLOCK]), ("BRAVO", &[BLOCK])]).arc();
        let pool = Pool::misbehaving("unavailable", 2, false, true);
        let (addr, _s, dir) = start_pool(model.clone(), pool.clone(), cap(1, 4)).await;
        let a = submit(addr, "ALPHA");
        let b = submit(addr, "BRAVO");
        for id in [&a, &b] {
            let done = finished(addr, id).await;
            assert_eq!(done["status"], "failed");
            assert_eq!(done["lifecycle"], Value::Null);
            assert_eq!(done["environment_id"], Value::Null);
            assert!(
                done["result"]["outcome_reason"]
                    .as_str()
                    .unwrap()
                    .contains("environment unavailable")
            );
            assert!(
                done["result"].get("measurement").is_none(),
                "no run, no measurement"
            );
            assert_eq!(events(addr, id), Vec::<Value>::new());
        }
        // No fallback, nothing asked, nothing run, no completion, and the slot was not leaked.
        assert_eq!(model.total_entered(), 0);
        assert!(!dir.join("alpha.txt").exists());
        let m = metrics(addr);
        assert_eq!(
            (
                m["failed_work"].as_u64(),
                m["started_work"].as_u64(),
                m["completed_work"].as_u64(),
                m["active_work"].as_u64()
            ),
            (Some(2), Some(0), Some(0), Some(0))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn queued_work_cancelled_before_admission_never_acquires_an_environment() {
        let model = Model::new(&[("ALPHA", &[BLOCK]), ("BRAVO", &[BLOCK])])
            .gated(&["ALPHA"])
            .arc();
        let pool = Pool::isolated("noacquire", 1);
        let (addr, _s, _d) = start_pool(model.clone(), pool.clone(), cap(1, 4)).await;
        let a = submit(addr, "ALPHA");
        until("A in the model", || model.entered("ALPHA") == 1).await;
        let b = submit(addr, "BRAVO");
        assert_eq!(
            call(addr, "POST", &format!("/v1/work/{b}/cancel"), None).status,
            200
        );
        model.release("ALPHA");
        finished(addr, &a).await;
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            pool.acquired.load(Ordering::SeqCst),
            1,
            "only A ever acquired one"
        );
        assert_eq!(get(addr, &b)["environment_id"], Value::Null);
        assert_eq!(model.entered("BRAVO"), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_work_that_panics_still_releases_its_environment() {
        let model = Model::new(&[("ALPHA", &[BLOCK]), ("BRAVO", &[BLOCK])])
            .panicking(&["ALPHA"])
            .arc();
        let pool = Pool::isolated("panic-release", 1);
        let (addr, _s, _d) = start_pool(model.clone(), pool.clone(), cap(1, 4)).await;
        let a = submit(addr, "ALPHA");
        let b = submit(addr, "BRAVO");
        assert_eq!(finished(addr, &a).await["status"], "failed");
        assert_eq!(
            finished(addr, &b).await["status"],
            "blocked",
            "the environment was free again"
        );
        assert_eq!(pool.acquired.load(Ordering::SeqCst), 2);
        assert_eq!(pool.released.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn more_concurrent_work_than_isolated_environments_is_refused_up_front() {
        let pool = Pool::misbehaving("capacity", 1, false, false);
        let err = Service::new(
            runtime(Model::new(&[]).arc()),
            Arc::new(Environments::new(pool)),
            true,
            cap(2, 4),
        )
        .err()
        .unwrap();
        assert!(err.contains("requires isolated environments"), "{err}");
    }

    // ---- cancellation of running work -----------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_running_work_is_advisory_and_reported_honestly() {
        let model = Model::new(&[("ALPHA", &[LIST, BLOCK])])
            .gated(&["ALPHA"])
            .arc();
        let (addr, _s, _d) = start(model.clone(), "cancel").await;
        let id = submit(addr, "ALPHA inspect");
        until("the model call", || model.entered("ALPHA") == 1).await;
        let c = call(addr, "POST", &format!("/v1/work/{id}/cancel"), None);
        assert_eq!(c.status, 200);
        assert_eq!(c.body["status"], "cancellation_requested");
        let g = get(addr, &id);
        assert_eq!(g["status"], "running", "requested is not cancelled");
        assert_eq!(g["cancellation_requested"], true);

        model.release("ALPHA");
        let done = finished(addr, &id).await;
        assert_ne!(done["status"], "cancelled");
        assert_eq!(done["cancellation_requested"], true);
        assert_eq!(model.calls("ALPHA"), 1, "no model call after the request");
        let ev = events(addr, &id);
        let k = kinds(&ev);
        assert!(
            k.iter().any(|x| x == "ExecutionCompleted"),
            "in-flight work was not interrupted"
        );
        assert!(
            ev.iter()
                .any(|e| e["kind"] == "ModelCalled" && e["succeeded"] == false)
        );
        assert!(
            !k.iter().any(|x| x.contains("ancel")),
            "no cancellation event is invented"
        );
        let again = call(addr, "POST", &format!("/v1/work/{id}/cancel"), None);
        assert_eq!(again.status, 409);
        assert_eq!(again.body["error"]["code"], "work_already_finished");
        assert_eq!(finished(addr, &id).await, done);
        assert_eq!(
            metrics(addr)["cancelled_work"],
            0,
            "only queue removals are cancelled"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_one_work_does_not_touch_another() {
        let model = Model::new(&[("ALPHA", &[LIST, BLOCK]), ("BRAVO", &[LIST, BLOCK])])
            .gated(&["ALPHA", "BRAVO"])
            .arc();
        let (addr, _s, _d) = start_with(model.clone(), "cancel-isolated", cap(2, 4)).await;
        let a = submit(addr, "ALPHA");
        let b = submit(addr, "BRAVO");
        until("both in the model", || {
            model.entered("ALPHA") == 1 && model.entered("BRAVO") == 1
        })
        .await;
        call(addr, "POST", &format!("/v1/work/{a}/cancel"), None);
        assert_eq!(get(addr, &b)["cancellation_requested"], false);
        model.release("ALPHA");
        model.release("BRAVO");
        finished(addr, &a).await;
        let done = finished(addr, &b).await;
        assert_eq!(
            model.calls("BRAVO"),
            2,
            "B's model was still asked after A's cancellation"
        );
        assert_eq!(done["cancellation_requested"], false);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn work_that_finishes_before_cancellation_keeps_its_own_terminal_state() {
        let (addr, _s, _d) = start(Model::new(&[("ALPHA", &[BLOCK])]).arc(), "terminal").await;
        let id = submit(addr, "ALPHA");
        let done = finished(addr, &id).await;
        assert_eq!(
            call(addr, "POST", &format!("/v1/work/{id}/cancel"), None).status,
            409
        );
        let after = get(addr, &id);
        assert_eq!(after, done);
        assert_eq!(after["cancellation_requested"], false);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn service_metrics_count_work_by_where_it_is_and_how_it_ended() {
        let model = Model::new(&[("ALPHA", &[BLOCK]), ("BRAVO", &[BLOCK])])
            .gated(&["ALPHA"])
            .arc();
        let (addr, _s, _d) = start_with(model.clone(), "metrics", cap(1, 4)).await;
        let empty = metrics(addr);
        for key in [
            "submitted_work",
            "queued_work",
            "started_work",
            "active_work",
            "completed_work",
            "blocked_work",
            "failed_work",
            "cancelled_work",
            "max_concurrent_work",
            "queue_wait_ms",
            "work_duration_ms",
        ] {
            assert!(empty.get(key).is_some(), "{key}");
        }
        assert_eq!(empty["max_concurrent_work"], 1);
        let a = submit(addr, "ALPHA");
        let b = submit(addr, "BRAVO");
        until("A to start", || model.entered("ALPHA") == 1).await;
        let m = metrics(addr);
        assert_eq!(
            (
                m["submitted_work"].as_u64(),
                m["active_work"].as_u64(),
                m["queued_work"].as_u64()
            ),
            (Some(2), Some(1), Some(1))
        );
        model.release("ALPHA");
        finished(addr, &a).await;
        finished(addr, &b).await;
        let m = metrics(addr);
        assert_eq!(
            (m["blocked_work"].as_u64(), m["started_work"].as_u64()),
            (Some(2), Some(2))
        );
        assert_eq!(m["queue_wait_ms"]["count"], 2);
        assert_eq!(m["work_duration_ms"]["count"], 2);
        assert_eq!(m["model_ms"]["count"], 2);
    }
}
