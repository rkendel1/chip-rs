//! `chip-cli serve [--host H] [--port P]`: the Chip Runtime Service.
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
//! - `POST /v1/work` `{"goal": "..."}` -> 202 `{"work_id", "status": "running"}`
//! - `GET  /v1/work/{id}`
//! - `GET  /v1/work/{id}/events`
//! - `POST /v1/work/{id}/cancel`
//!
//! The client supplies a goal and nothing else: provider, model, endpoint, executable, workspace
//! root, ids, receipts, observations and evidence are all runtime concerns, and a request that
//! names any of them is refused, not ignored.
//!
//! Exit status: 0 normal end (the service runs until it is stopped), 2 usage, 3 required
//! infrastructure unavailable (no model selected, no usable PAX; nothing ran), 4 could not listen.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chip_core::{CapabilityEvent, ExecutionEvent, WorkEvent, WorkId, WorkLimits};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::software_work::{
    DEFAULT_MAX_EXECUTIONS, DEFAULT_MAX_TURNS, SoftwareWork, WorkRuntime, goal_is_acceptable,
    render_json,
};
use crate::verify::{EXIT_RUNTIME_FAILURE, EXIT_UNAVAILABLE, EXIT_USAGE};

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8765;

/// Work that may be running at once. A resource bound, not a queue: past it, a request is refused.
const MAX_RUNNING: usize = 8;
const MAX_HEAD_BYTES: usize = 8 * 1024;
/// A goal is at most 2000 bytes; this leaves room for the JSON around it and nothing more.
const MAX_BODY_BYTES: usize = 8 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

// ---- the registry ---------------------------------------------------------------------------------

/// The registry's own knowledge of one work item. While it runs the registry knows only that it
/// was started and whether cancellation was requested: the loop reports its events and outcome
/// when it ends, and until then there is nothing authoritative to show.
enum Phase {
    Running,
    Finished(Box<Finished>),
}

/// What the runtime established, projected once when the loop returned.
struct Finished {
    /// `TerminalState::name()`, or `failed` if the task running the loop ended abnormally.
    state: &'static str,
    /// The CLI's JSON report for this work (`render_json`), unchanged.
    result: Value,
    events: Vec<Value>,
}

struct Item {
    goal: String,
    cancel: Arc<AtomicBool>,
    phase: Mutex<Phase>,
}

pub struct Service {
    runtime: Arc<WorkRuntime>,
    limits: WorkLimits,
    items: Mutex<HashMap<String, Arc<Item>>>,
    running: AtomicUsize,
    issued: AtomicU64,
    /// Only a loopback listener can check `Host`: a client that names another host is a browser
    /// being steered to this port by a name that is not ours.
    loopback_hosts_only: bool,
}

impl Service {
    pub fn new(runtime: Arc<WorkRuntime>, loopback_hosts_only: bool) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            limits: WorkLimits {
                max_turns: DEFAULT_MAX_TURNS,
                max_executions: DEFAULT_MAX_EXECUTIONS,
            },
            items: Mutex::new(HashMap::new()),
            running: AtomicUsize::new(0),
            issued: AtomicU64::new(0),
            loopback_hosts_only,
        })
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
            if !self.items.lock().unwrap().contains_key(&id) {
                return id;
            }
        }
    }

    fn item(&self, id: &str) -> Option<Arc<Item>> {
        self.items.lock().unwrap().get(id).cloned()
    }

    fn start(self: &Arc<Self>, goal: String) -> Result<String, Response> {
        if self.running.fetch_add(1, Ordering::SeqCst) >= MAX_RUNNING {
            self.running.fetch_sub(1, Ordering::SeqCst);
            return Err(Response::error(
                429,
                "too_many_running",
                "The service is already running as much work as it allows",
            ));
        }
        let id = self.allocate_id();
        let cancel = Arc::new(AtomicBool::new(false));
        let item = Arc::new(Item {
            goal: goal.clone(),
            cancel: cancel.clone(),
            phase: Mutex::new(Phase::Running),
        });
        self.items.lock().unwrap().insert(id.clone(), item.clone());

        // The existing runtime does the work. The inner task exists so a panic in it is observed
        // and reported as a failure instead of leaving the work "running" forever.
        let runtime = self.runtime.clone();
        let limits = self.limits;
        let work_id = WorkId::new(id.clone());
        let task =
            tokio::spawn(async move { runtime.run(work_id, &goal, limits, Some(cancel)).await.0 });
        let service = self.clone();
        tokio::spawn(async move {
            let finished = match task.await {
                Ok(work) => finish(&work, &service.runtime),
                Err(_) => Finished {
                    state: "failed",
                    result: json!({
                        "outcome_reason": "the task running the work ended abnormally",
                    }),
                    events: Vec::new(),
                },
            };
            *item.phase.lock().unwrap() = Phase::Finished(Box::new(finished));
            service.running.fetch_sub(1, Ordering::SeqCst);
        });
        Ok(id)
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
            (["v1", "work"], "POST") => self.post_work(&request),
            (["v1", "work", id], "GET") => self.get_work(id),
            (["v1", "work", id, "events"], "GET") => self.get_events(id),
            (["v1", "work", id, "cancel"], "POST") => self.cancel(id),
            (["health"], _)
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
        let goal = match parse_goal(&request.body) {
            Ok(goal) => goal,
            Err(response) => return response,
        };
        match self.start(goal) {
            Ok(id) => Response::ok(202, json!({"work_id": id, "status": "running"})),
            Err(response) => response,
        }
    }

    fn get_work(&self, id: &str) -> Response {
        let Some(item) = self.item(id) else {
            return unknown_work();
        };
        let phase = item.phase.lock().unwrap();
        let cancellation_requested = item.cancel.load(Ordering::SeqCst);
        let mut body = match &*phase {
            Phase::Running => json!({"status": "running", "lifecycle": "executing"}),
            Phase::Finished(f) => json!({
                "status": f.state,
                "lifecycle": f.state,
                "result": f.result,
            }),
        };
        body["work_id"] = json!(id);
        body["goal"] = json!(item.goal);
        body["cancellation_requested"] = json!(cancellation_requested);
        Response::ok(200, body)
    }

    fn get_events(&self, id: &str) -> Response {
        let Some(item) = self.item(id) else {
            return unknown_work();
        };
        let phase = item.phase.lock().unwrap();
        let (complete, events) = match &*phase {
            // The loop returns its trajectory when it ends. Nothing is invented in the meantime.
            Phase::Running => (false, Vec::new()),
            Phase::Finished(f) => (true, f.events.clone()),
        };
        Response::ok(
            200,
            json!({"work_id": id, "complete": complete, "events": events}),
        )
    }

    fn cancel(&self, id: &str) -> Response {
        let Some(item) = self.item(id) else {
            return unknown_work();
        };
        let phase = item.phase.lock().unwrap();
        if let Phase::Finished(f) = &*phase {
            return Response::error(
                409,
                "work_already_finished",
                &format!("The work already ended: {}", f.state),
            );
        }
        // Advisory: it stops the next model call. Nothing in flight is interrupted, the runtime
        // may still complete the work, and the outcome is whatever the runtime then reports.
        item.cancel.store(true, Ordering::SeqCst);
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

fn unknown_work() -> Response {
    Response::error(404, "work_not_found", "Unknown work id")
}

/// A goal, from a body that is exactly `{"goal": "<text>"}`. Anything else is refused: a field
/// naming an id, receipt, observation, executable or configuration is an attempt to supply
/// authority, and it is rejected rather than ignored.
fn parse_goal(body: &[u8]) -> Result<String, Response> {
    let bad = |code: &str, message: &str| Err(Response::error(400, code, message));
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(body) else {
        return bad("malformed_request", "The body must be a JSON object");
    };
    if let Some(name) = fields.keys().find(|k| k.as_str() != "goal") {
        let shown: String = name.chars().filter(|c| !c.is_control()).take(40).collect();
        return bad(
            "unknown_field",
            &format!("Only `goal` is accepted; `{shown}` is a runtime concern"),
        );
    }
    match fields.get("goal") {
        None => bad("missing_goal", "A goal is required"),
        Some(Value::String(goal)) if goal.trim().is_empty() => {
            bad("empty_goal", "The goal must not be empty")
        }
        Some(Value::String(goal)) if goal_is_acceptable(goal) => Ok(goal.trim().to_string()),
        Some(Value::String(_)) => bad(
            "invalid_goal",
            "The goal must be plain text of at most 2000 bytes",
        ),
        Some(_) => bad("malformed_request", "The goal must be a string"),
    }
}

// ---- projecting the runtime's report ---------------------------------------------------------------

fn finish(work: &SoftwareWork, runtime: &WorkRuntime) -> Finished {
    let result =
        serde_json::from_str(&render_json(work, &runtime.pax)).expect("the work report is JSON");
    Finished {
        state: work.report.outcome.terminal_state().name(),
        result,
        events: work.report.events.iter().map(event_json).collect(),
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
    eprintln!("usage: chip-cli serve [--host ADDR] [--port PORT]");
    eprintln!(
        "       defaults: {DEFAULT_HOST}:{DEFAULT_PORT}. The model comes from CHIP_PROVIDER / CHIP_MODEL / CHIP_ENDPOINT, as for `work`; works on the project in the current directory"
    );
    EXIT_USAGE
}

fn parse_args(args: &[String]) -> Result<SocketAddr, i32> {
    let (mut host, mut port) = (DEFAULT_HOST.to_string(), DEFAULT_PORT);
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
    Ok(SocketAddr::new(ip, port))
}

pub async fn serve(args: &[String]) -> i32 {
    let addr = match parse_args(args) {
        Ok(addr) => addr,
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
    let runtime = match prepare(&root, context_budget).await {
        Ok(runtime) => runtime,
        Err(why) => {
            eprintln!("error: {why}");
            return EXIT_UNAVAILABLE;
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
    println!("Chip Runtime Service listening on http://{bound}");
    println!(
        "Local trusted-client interface: no authentication, no CORS. Work is held in memory for this process only."
    );
    let _ = std::io::Write::flush(&mut std::io::stdout());
    run(listener, Service::new(runtime, bound.ip().is_loopback())).await;
    0
}

async fn prepare(root: &Path, context_budget: Option<usize>) -> Result<Arc<WorkRuntime>, String> {
    let selection = crate::provider_selection::Selection::default();
    WorkRuntime::prepare(&selection, context_budget, root)
        .await
        .map(Arc::new)
}

#[cfg(test)]
mod tests {
    //! The service over the real work loop, real project capabilities and a scripted model, driven
    //! through real TCP. PAX is not involved: no script here runs `pax.test`, so no work in this
    //! module can be verified or complete, and none claims to.

    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::path::PathBuf;

    use chip_pax::ResolvedPax;
    use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};
    use tokio::sync::Semaphore;

    use super::*;

    const LIST: &str =
        r#"{"decision":"request_capability","capability":"project.list","inputs":{"path":"."}}"#;
    const BLOCK: &str = r#"{"decision":"block","reason":"scripted stop"}"#;
    /// An invented input on a capability that does not declare it.
    const FORGED: &str = r#"{"decision":"request_capability","capability":"project.list","inputs":{"path":".","executable":"/bin/sh"}}"#;

    /// Replies by which marker the goal carries, so concurrent works never share a script.
    struct Model {
        scripts: Mutex<HashMap<&'static str, VecDeque<&'static str>>>,
        gate: Semaphore,
        entered: AtomicUsize,
        calls: AtomicUsize,
    }

    impl Model {
        fn new(scripts: &[(&'static str, &[&'static str])]) -> Arc<Self> {
            Arc::new(Self {
                scripts: Mutex::new(
                    scripts
                        .iter()
                        .map(|(m, r)| (*m, r.iter().copied().collect()))
                        .collect(),
                ),
                gate: Semaphore::new(0),
                entered: AtomicUsize::new(0),
                calls: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for Model {
        async fn complete(&self, r: ModelRequest) -> Result<ModelResponse, FxError> {
            let text: String = r.messages.iter().map(|m| m.content.as_str()).collect();
            self.entered.fetch_add(1, Ordering::SeqCst);
            if text.contains("SLOW") {
                self.gate.acquire().await.unwrap().forget();
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            let reply = {
                let mut scripts = self.scripts.lock().unwrap();
                let queue = scripts
                    .iter_mut()
                    .find(|(marker, _)| text.contains(**marker))
                    .map(|(_, q)| q);
                queue.and_then(|q| q.pop_front())
            };
            let reply = reply.ok_or_else(|| FxError::Provider("unscripted model call".into()))?;
            Ok(ModelResponse::new("m", reply, Usage::new(3, 2)))
        }
    }

    fn root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chip-serve-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn one() -> u8 { 1 }\n").unwrap();
        dir
    }

    fn runtime(model: Arc<Model>, dir: &Path) -> Arc<WorkRuntime> {
        Arc::new(WorkRuntime::for_test(
            model,
            dir,
            ResolvedPax {
                path: PathBuf::from("/nonexistent/pax"),
                version: "0.0.0".into(),
            },
        ))
    }

    async fn start(model: Arc<Model>, tag: &str) -> (SocketAddr, Arc<Service>, PathBuf) {
        let dir = root(tag);
        let service = Service::new(runtime(model, &dir), true);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(run(listener, service.clone()));
        (addr, service, dir)
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

    async fn finished(addr: SocketAddr, id: &str) -> Value {
        for _ in 0..500 {
            let r = call(addr, "GET", &format!("/v1/work/{id}"), None);
            assert_eq!(r.status, 200);
            if r.body["status"] != "running" {
                return r.body;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
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

    #[test]
    fn the_defaults_are_loopback_8765_and_flags_override_them() {
        let none: Vec<String> = Vec::new();
        assert_eq!(parse_args(&none), Ok("127.0.0.1:8765".parse().unwrap()));
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            parse_args(&a(&["--host", "127.0.0.1", "--port", "9000"])),
            Ok("127.0.0.1:9000".parse().unwrap())
        );
        assert!(parse_args(&a(&["--port", "x"])).is_err());
        assert!(parse_args(&a(&["--host", "not-an-ip"])).is_err());
        assert!(parse_args(&a(&["--model", "m"])).is_err());
        assert!(parse_args(&a(&["--port"])).is_err());
        assert!(DEFAULT_HOST.parse::<IpAddr>().unwrap().is_loopback());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn health_is_service_health_and_nothing_more() {
        let (addr, _s, _d) = start(Model::new(&[]), "health").await;
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
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_loopback_host_names_are_answered() {
        let (addr, _s, _d) = start(Model::new(&[]), "host").await;
        let r = raw(addr, b"GET /health HTTP/1.1\r\nHost: evil.example\r\n\r\n");
        assert_eq!(r.status, 403);
        assert_eq!(r.body["error"]["code"], "host_not_allowed");
        let r = raw(addr, b"GET /health HTTP/1.1\r\n\r\n");
        assert_eq!(r.status, 403);
        let r = raw(
            addr,
            b"GET /health HTTP/1.1\r\nHost: localhost:8765\r\n\r\n",
        );
        assert_eq!(r.status, 200);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn malformed_requests_are_refused_and_start_nothing() {
        let model = Model::new(&[]);
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
        let r = raw(addr, h(&huge).as_bytes());
        assert_eq!(r.status, 413);
        // Not JSON by type: also refused, whatever the body says.
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
        assert!(service.items.lock().unwrap().is_empty());
        assert_eq!(
            model.entered.load(Ordering::SeqCst),
            0,
            "no model was asked"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_cannot_supply_authority_or_configuration() {
        let model = Model::new(&[]);
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
        ] {
            let body = format!(r#"{{"goal":"inspect the project","{field}":"forged"}}"#);
            let r = call(addr, "POST", "/v1/work", Some(&body));
            assert_eq!(r.status, 400, "{field}");
            assert_eq!(r.body["error"]["code"], "unknown_field", "{field}");
        }
        // Nothing was created, asked, or executed, and the project is untouched.
        assert!(service.items.lock().unwrap().is_empty());
        assert_eq!(model.entered.load(Ordering::SeqCst), 0);
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "pub fn one() -> u8 { 1 }\n"
        );
        // The read-only routes take no body that changes anything either.
        let r = call(addr, "POST", "/v1/work/work_x/events", Some("{}"));
        assert_eq!(r.status, 405);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn post_is_prompt_the_id_is_chips_and_the_loop_runs_in_the_background() {
        let model = Model::new(&[("SLOW", &[LIST, BLOCK])]);
        let (addr, _s, _d) = start(model.clone(), "prompt").await;
        let t0 = std::time::Instant::now();
        let r = post(addr, "SLOW inspect the project");
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "POST waited for the work"
        );
        assert_eq!(r.status, 202);
        assert_eq!(r.body["status"], "running");
        let id = r.body["work_id"].as_str().unwrap().to_string();
        assert!(id.starts_with("work_"), "{id}");
        assert_ne!(id, "m", "not the provider's response id");

        // The model is held inside its first call: the work is running, and says only that.
        while model.entered.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let g = call(addr, "GET", &format!("/v1/work/{id}"), None);
        assert_eq!(g.status, 200);
        assert_eq!(g.body["status"], "running");
        assert_eq!(g.body["lifecycle"], "executing");
        assert_eq!(g.body["goal"], "SLOW inspect the project");
        assert_eq!(g.body["cancellation_requested"], false);
        assert!(
            g.body.get("result").is_none(),
            "no result before there is one"
        );
        let e = call(addr, "GET", &format!("/v1/work/{id}/events"), None);
        assert_eq!(e.body["complete"], false);
        assert_eq!(
            e.body["events"],
            json!([]),
            "no event is invented while it runs"
        );

        model.gate.add_permits(10);
        let done = finished(addr, &id).await;
        assert_eq!(done["status"], "blocked");
        assert_eq!(done["lifecycle"], "blocked");
        assert_eq!(done["result"]["terminal_state"], "blocked");
        assert_eq!(done["result"]["verified"], false);
        assert_eq!(done["result"]["audit"]["clean"], true);
        // Terminal state is authoritative and stable.
        assert_eq!(finished(addr, &id).await, done);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn events_are_the_runtimes_trajectory_in_order_and_match_a_direct_run() {
        let script: &[(&str, &[&str])] = &[("TRACE", &[LIST, BLOCK])];
        let (addr, _s, dir) = start(Model::new(script), "events").await;
        let id = post(addr, "TRACE inspect the project").body["work_id"]
            .as_str()
            .unwrap()
            .to_string();
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
        assert_eq!(k.iter().filter(|x| *x == "ExecutionStarted").count(), 1);
        assert_eq!(k.iter().filter(|x| *x == "WorkBlocked").count(), 1);
        assert_eq!(
            k.last().unwrap(),
            "WorkBlocked",
            "nothing after the terminal event"
        );

        // The same runtime, called the way `chip work` calls it, records the same trajectory.
        let direct = runtime(Model::new(script), &dir)
            .run(
                WorkId::new("direct"),
                "TRACE inspect the project",
                WorkLimits {
                    max_turns: DEFAULT_MAX_TURNS,
                    max_executions: DEFAULT_MAX_EXECUTIONS,
                },
                None,
            )
            .await
            .0;
        let direct: Vec<Value> = direct.report.events.iter().map(event_json).collect();
        assert_eq!(served, direct);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rejected_request_produces_no_execution_observation_or_evidence() {
        let (addr, _s, _d) = start(Model::new(&[("FORGE", &[FORGED])]), "rejected").await;
        let id = post(addr, "FORGE something").body["work_id"]
            .as_str()
            .unwrap()
            .to_string();
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
    async fn unknown_work_is_404_on_every_route() {
        let (addr, _s, _d) = start(Model::new(&[]), "unknown").await;
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

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_work_stays_isolated() {
        let script: &[(&str, &[&str])] = &[
            ("ALPHA", &[LIST, BLOCK]),
            ("BETA", &[FORGED]),
            ("GAMMA", &[BLOCK]),
        ];
        let (addr, _s, _d) = start(Model::new(script), "isolated").await;
        let goals = ["ALPHA one", "BETA two", "GAMMA three"];
        let mut ids = Vec::new();
        for goal in &goals {
            ids.push(
                post(addr, goal).body["work_id"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            );
        }
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "ids are unique");
        for (id, goal) in ids.iter().zip(&goals) {
            let done = finished(addr, id).await;
            assert_eq!(done["work_id"], id.as_str());
            assert_eq!(done["goal"], *goal);
            assert_eq!(done["result"]["goal"], *goal);
            let ev = events(addr, id);
            assert_eq!(ev[0]["kind"], "WorkStarted");
            assert!(ev[0]["goal"].as_str().unwrap().contains(goal), "{goal}");
        }
        let shape = |i: usize| kinds(&events(addr, &ids[i]));
        assert!(shape(0).iter().any(|k| k == "ExecutionStarted"));
        assert!(!shape(1).iter().any(|k| k == "ExecutionStarted"));
        assert!(!shape(2).iter().any(|k| k == "ExecutionStarted"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_is_advisory_and_reported_honestly() {
        let model = Model::new(&[("SLOW", &[LIST, BLOCK])]);
        let (addr, _s, _d) = start(model.clone(), "cancel").await;
        let id = post(addr, "SLOW inspect").body["work_id"]
            .as_str()
            .unwrap()
            .to_string();
        while model.entered.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let c = call(addr, "POST", &format!("/v1/work/{id}/cancel"), None);
        assert_eq!(c.status, 200);
        assert_eq!(c.body["status"], "cancellation_requested");
        assert_ne!(c.body["status"], "cancelled");
        // Requested is not cancelled: the work is still running, and says so.
        let g = call(addr, "GET", &format!("/v1/work/{id}"), None);
        assert_eq!(g.body["status"], "running");
        assert_eq!(g.body["cancellation_requested"], true);

        // The model call already in flight completes and its request executes (nothing is
        // interrupted); the next model call is the one that is not made.
        model.gate.add_permits(10);
        let done = finished(addr, &id).await;
        assert_ne!(done["status"], "cancelled");
        assert_eq!(done["cancellation_requested"], true);
        assert_eq!(
            model.calls.load(Ordering::SeqCst),
            1,
            "no model call after the request"
        );
        let ev = events(addr, &id);
        let k = kinds(&ev);
        assert!(k.iter().any(|x| x == "ExecutionCompleted"), "{k:?}");
        assert!(
            ev.iter()
                .any(|e| e["kind"] == "ModelCalled" && e["succeeded"] == false),
            "{k:?}"
        );
        assert!(
            !k.iter().any(|x| x.contains("ancel")),
            "no cancellation event is invented"
        );
        assert!(done["result"].get("cancel_receipt").is_none());

        // Finished work cannot be cancelled, and nothing changes its state.
        let again = call(addr, "POST", &format!("/v1/work/{id}/cancel"), None);
        assert_eq!(again.status, 409);
        assert_eq!(again.body["error"]["code"], "work_already_finished");
        assert_eq!(finished(addr, &id).await, done);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn work_that_finishes_before_cancellation_keeps_its_own_terminal_state() {
        let (addr, _s, _d) = start(Model::new(&[("QUICK", &[BLOCK])]), "terminal").await;
        let id = post(addr, "QUICK").body["work_id"]
            .as_str()
            .unwrap()
            .to_string();
        let done = finished(addr, &id).await;
        let c = call(addr, "POST", &format!("/v1/work/{id}/cancel"), None);
        assert_eq!(c.status, 409);
        let after = call(addr, "GET", &format!("/v1/work/{id}"), None).body;
        assert_eq!(after, done);
        assert_eq!(after["cancellation_requested"], false);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn responses_carry_no_credentials_environment_or_host_paths() {
        let (addr, _s, dir) = start(Model::new(&[("TRACE", &[LIST, BLOCK])]), "secrets").await;
        let id = post(addr, "TRACE inspect").body["work_id"]
            .as_str()
            .unwrap()
            .to_string();
        finished(addr, &id).await;
        let all = format!(
            "{}{}",
            call(addr, "GET", &format!("/v1/work/{id}"), None).body,
            call(addr, "GET", &format!("/v1/work/{id}/events"), None).body
        );
        assert!(
            !all.contains(dir.to_str().unwrap()),
            "host path leaked: {all}"
        );
        for name in ["API_KEY", "TOKEN", "SECRET", "Authorization"] {
            assert!(!all.contains(name), "{name} in {all}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_service_refuses_work_past_its_bound() {
        let model = Model::new(&[("SLOW", &[BLOCK; MAX_RUNNING + 1])]);
        let (addr, _s, _d) = start(model.clone(), "bound").await;
        for _ in 0..MAX_RUNNING {
            assert_eq!(post(addr, "SLOW work").status, 202);
        }
        let r = post(addr, "SLOW one too many");
        assert_eq!(r.status, 429);
        assert_eq!(r.body["error"]["code"], "too_many_running");
        model.gate.add_permits(100);
    }
}
