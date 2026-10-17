//! The host-driven session: Chip's work loop suspended on the host's two answers.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use async_trait::async_trait;
use chip_core::{
    Agent, CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId,
    CapabilityInput, CapabilityProvider, ExecutionError, ExecutionId, ExecutionObserver,
    ExecutionRequest, ExecutionResult, ExecutionStatus, Executor, InputValue,
    ModelDecisionBoundary, NoLocalPolicy, WorkGoal, WorkId, WorkLimits, WorkReport, WorkSpec,
    verify_trajectory,
};
use chip_fx::mapping::{OptionsPolicy, RequestIds, to_fx_request};
use chip_fx::report::from_report_json;
use chip_inference::{FxError, ModelProvider, ModelRequest, ModelResponse};
use fx_core::RequestId;
use serde_json::{Map, Value, json};

/// The host protocol version, exported as `chip_protocol_version`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest command the session will parse.
const MAX_COMMAND_BYTES: usize = 16 * 1024 * 1024;

const CANCELLED_BEFORE_CALL: &str = "cancellation was requested; no further model call was made";

/// What the work is waiting for.
enum Pending {
    Inference { request: Value, id: RequestId },
    Execution(ExecutionRequest),
}

/// The meeting point between the suspended work and the host's answers.
#[derive(Default)]
struct Exchange {
    pending: Option<Pending>,
    inflight_inference: Option<RequestId>,
    inference: Option<Result<ModelResponse, FxError>>,
    execution: Option<Result<ExecutionResult, ExecutionError>>,
    cancelled: bool,
}

fn lock(exchange: &Mutex<Exchange>) -> std::sync::MutexGuard<'_, Exchange> {
    exchange
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A future that is ready once the host has answered. It never registers a waker: the host
/// polls the work again when it has an answer.
struct Answer<T> {
    exchange: Arc<Mutex<Exchange>>,
    take: fn(&mut Exchange) -> Option<T>,
}

impl<T> Future for Answer<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<T> {
        match (self.take)(&mut lock(&self.exchange)) {
            Some(answer) => Poll::Ready(answer),
            None => Poll::Pending,
        }
    }
}

/// Chip's model port, served by the host through FX.
struct HostModel {
    exchange: Arc<Mutex<Exchange>>,
    ids: Mutex<RequestIds>,
    policy: OptionsPolicy,
    budget: Option<Duration>,
}

#[async_trait]
impl ModelProvider for HostModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, FxError> {
        if lock(&self.exchange).cancelled {
            // The same rule as the native CLI: a cancelled work makes no further model call.
            return Err(FxError::Provider(CANCELLED_BEFORE_CALL.to_owned()));
        }
        let id = self
            .ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .issue();
        let fx_request = to_fx_request(&request, id, self.policy, self.budget)?;
        let document: Value = serde_json::from_slice(&fx_core::encode_request(&fx_request))
            .map_err(|_| FxError::Serialization("the FX request does not encode".to_owned()))?;
        {
            let mut exchange = lock(&self.exchange);
            exchange.pending = Some(Pending::Inference {
                request: document,
                id,
            });
            exchange.inflight_inference = Some(id);
        }
        Answer {
            exchange: self.exchange.clone(),
            take: |exchange| exchange.inference.take(),
        }
        .await
    }
}

/// Chip's executor, served by the host's explicit capability.
struct HostExecutor {
    exchange: Arc<Mutex<Exchange>>,
}

#[async_trait]
impl Executor for HostExecutor {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionError> {
        lock(&self.exchange).pending = Some(Pending::Execution(request));
        Answer {
            exchange: self.exchange.clone(),
            take: |exchange| exchange.execution.take(),
        }
        .await
    }
}

/// The capabilities the host declared for this work.
struct DeclaredCapabilities(Vec<CapabilityDescriptor>);

#[async_trait]
impl CapabilityProvider for DeclaredCapabilities {
    async fn capabilities(&self) -> Result<Vec<CapabilityDescriptor>, CapabilityError> {
        Ok(self.0.clone())
    }

    async fn availability(&self, id: &CapabilityId) -> CapabilityAvailability {
        if self.0.iter().any(|descriptor| &descriptor.id == id) {
            CapabilityAvailability::Available
        } else {
            CapabilityAvailability::Unavailable("not declared".to_owned())
        }
    }
}

struct Running {
    work: Pin<Box<dyn Future<Output = WorkReport>>>,
    exchange: Arc<Mutex<Exchange>>,
    /// The response for what the work is waiting on, kept so a repeated command is idempotent.
    current: Vec<u8>,
    waiting_on: Option<Waiting>,
    execution_id: Option<ExecutionId>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Waiting {
    Inference,
    Execution,
}

/// One work, driven by the host.
#[derive(Default)]
pub struct Session {
    running: Option<Running>,
    finished: bool,
}

fn respond(value: &Value) -> Vec<u8> {
    value.to_string().into_bytes()
}

fn error(code: &str, message: &str) -> Vec<u8> {
    respond(&json!({"status": "error", "error": {"code": code, "message": message}}))
}

fn text<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn seed(hex: &str) -> Option<[u8; 8]> {
    if hex.len() != 16
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let mut out = [0u8; 8];
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn capability(value: &Value) -> Option<CapabilityDescriptor> {
    let object = value.as_object()?;
    let mut descriptor = CapabilityDescriptor::new(
        CapabilityId::new(text(object, "id")?).ok()?,
        text(object, "name")?,
        object
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    for input in object
        .get("inputs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let input = input.as_object()?;
        descriptor.inputs.push(CapabilityInput {
            name: text(input, "name")?.to_owned(),
            description: input
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            required: input
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        });
    }
    Some(descriptor)
}

fn input_json(value: &InputValue) -> Value {
    match value {
        InputValue::Text(text) => json!(text),
        InputValue::Integer(number) => json!(number),
        InputValue::Bool(flag) => json!(flag),
    }
}

fn done_json(report: &WorkReport, limits: &WorkLimits) -> Value {
    let measurement = report.measurement();
    let mut result = Map::new();
    result.insert(
        "state".into(),
        json!(report.outcome.terminal_state().name()),
    );
    match &report.outcome {
        chip_core::WorkOutcome::Completed { summary } => {
            result.insert("summary".into(), json!(summary));
        }
        chip_core::WorkOutcome::Escalated { reason }
        | chip_core::WorkOutcome::Blocked { reason } => {
            result.insert("reason".into(), json!(reason));
        }
        chip_core::WorkOutcome::Failed { reason, delivery } => {
            result.insert("reason".into(), json!(reason));
            if let Some(delivery) = delivery {
                result.insert("delivery".into(), json!(delivery.as_str()));
            }
        }
        chip_core::WorkOutcome::LimitReached { limit } => {
            result.insert("limit".into(), json!(format!("{limit:?}")));
        }
    }
    result.insert(
        "measurement".into(),
        json!({
            "turns": measurement.turns,
            "executions": measurement.executions,
            "observations": measurement.observations,
            "model_calls": measurement.model_calls,
            "model_escalations": measurement.model_escalations,
            "evidence_hits": measurement.evidence_hits,
            "local_decisions": measurement.local_decisions,
            "model_tokens": measurement.model_tokens,
        }),
    );
    let names: Vec<String> = report
        .events
        .iter()
        .map(|event| {
            let debug = format!("{event:?}");
            debug
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .next()
                .unwrap_or("")
                .to_owned()
        })
        .collect();
    result.insert("events".into(), json!(names));
    result.insert(
        "trajectory_problems".into(),
        json!(verify_trajectory(&report.events, limits)),
    );
    json!({"status": "done", "result": result})
}

impl Session {
    /// An empty session. `begin` starts the work.
    pub fn new() -> Session {
        Session::default()
    }

    /// Starts the work from its first document and runs it to its first suspension.
    pub fn begin(&mut self, input: &[u8]) -> Vec<u8> {
        if self.running.is_some() || self.finished {
            return error("already_begun", "this session already has a work");
        }
        if input.len() > MAX_COMMAND_BYTES {
            return error(
                "too_large",
                "the document is larger than the session accepts",
            );
        }
        let Ok(Value::Object(document)) = serde_json::from_slice::<Value>(input) else {
            return error(
                "invalid_document",
                "the begin document is not a JSON object",
            );
        };
        if document.get("v").and_then(Value::as_u64) != Some(u64::from(PROTOCOL_VERSION)) {
            return error("unsupported_version", "unsupported protocol version");
        }
        let (Some(work_id), Some(goal), Some(model)) = (
            text(&document, "work_id"),
            text(&document, "goal"),
            text(&document, "model"),
        ) else {
            return error(
                "invalid_document",
                "work_id, goal and model are required strings",
            );
        };
        let Some(seed) = text(&document, "request_seed").and_then(seed) else {
            return error(
                "invalid_document",
                "request_seed must be 16 lowercase hex digits",
            );
        };
        let policy = match text(&document, "options_policy") {
            Some("refuse") => OptionsPolicy::Refuse,
            Some("drop_unsupported") => OptionsPolicy::DropUnsupported,
            _ => {
                return error(
                    "invalid_document",
                    "options_policy must be \"refuse\" or \"drop_unsupported\"",
                );
            }
        };
        let mut descriptors = Vec::new();
        for value in document
            .get("capabilities")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(descriptor) = capability(value) else {
                return error("invalid_document", "a capability declaration is malformed");
            };
            descriptors.push(descriptor);
        }
        let mut limits = WorkLimits::default();
        if let Some(Value::Object(requested)) = document.get("limits") {
            if let Some(turns) = requested.get("max_turns").and_then(Value::as_u64) {
                limits.max_turns = usize::try_from(turns).unwrap_or(usize::MAX);
            }
            if let Some(executions) = requested.get("max_executions").and_then(Value::as_u64) {
                limits.max_executions = usize::try_from(executions).unwrap_or(usize::MAX);
            }
        }
        let budget = document
            .get("budget_ms")
            .and_then(Value::as_u64)
            .map(Duration::from_millis);

        let exchange = Arc::new(Mutex::new(Exchange::default()));
        let model_port = HostModel {
            exchange: exchange.clone(),
            ids: Mutex::new(RequestIds::new(seed)),
            policy,
            budget,
        };
        let agent = Agent::with_model(Arc::new(model_port), model.to_owned())
            .with_capabilities(Arc::new(DeclaredCapabilities(descriptors)))
            .with_executor(Arc::new(HostExecutor {
                exchange: exchange.clone(),
            }))
            .with_observer(Arc::new(ExecutionObserver));
        let mut spec = WorkSpec::new(WorkId::new(work_id), WorkGoal::new(goal));
        spec.limits = limits.clone();
        let work = Box::pin(async move {
            agent
                .run_work(&spec, &NoLocalPolicy, &ModelDecisionBoundary)
                .await
        });
        self.running = Some(Running {
            work,
            exchange,
            current: Vec::new(),
            waiting_on: None,
            execution_id: None,
        });
        self.advance(&limits)
    }

    /// Applies one JSON command: `inference_result`, `execution_result` or `cancel`.
    pub fn command(&mut self, input: &[u8]) -> Vec<u8> {
        if self.finished {
            return respond(&json!({"status": "done", "ignored": true}));
        }
        if self.running.is_none() {
            return error("not_begun", "the session has no work yet");
        }
        if input.len() > MAX_COMMAND_BYTES {
            return error(
                "too_large",
                "the command is larger than the session accepts",
            );
        }
        let Ok(Value::Object(command)) = serde_json::from_slice::<Value>(input) else {
            return error("invalid_command", "the command is not a JSON object");
        };
        let name = text(&command, "cmd").unwrap_or("").to_owned();
        let limits = WorkLimits::default();
        let running = self.running.as_mut().expect("checked above");
        match name.as_str() {
            "cancel" => {
                lock(&running.exchange).cancelled = true;
                // Idempotent: report again what the work is waiting on.
                running.current.clone()
            }
            "inference_result" => {
                if running.waiting_on != Some(Waiting::Inference) {
                    return error(
                        "unexpected_command",
                        "the work is not waiting for an inference",
                    );
                }
                let answer = match (command.get("host_error"), command.get("report")) {
                    (Some(Value::String(message)), None) => Err(FxError::Provider(message.clone())),
                    (None, Some(report)) => {
                        let id = lock(&running.exchange)
                            .inflight_inference
                            .take()
                            .unwrap_or(RequestId([0; 16]));
                        from_report_json(&report.to_string(), id)
                    }
                    _ => {
                        return error(
                            "invalid_command",
                            "give exactly one of report or host_error",
                        );
                    }
                };
                lock(&running.exchange).inference = Some(answer);
                self.advance(&limits)
            }
            "execution_result" => {
                if running.waiting_on != Some(Waiting::Execution) {
                    return error(
                        "unexpected_command",
                        "the work is not waiting for an execution",
                    );
                }
                let Some(id) = running.execution_id.clone() else {
                    return error("invalid_command", "no execution is pending");
                };
                let output = command
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let answer = match (text(&command, "status"), text(&command, "error")) {
                    (Some("success"), None) => {
                        let mut result = ExecutionResult::success(id, output);
                        if let Some(receipt) = text(&command, "receipt_id") {
                            result = result.with_receipt_id(receipt);
                        }
                        Ok(result)
                    }
                    (Some("failure"), None) => Ok(ExecutionResult::failure(id, output)),
                    (Some("cancelled"), None) => Ok(ExecutionResult {
                        id,
                        status: ExecutionStatus::Cancelled,
                        output,
                        receipt_id: None,
                        evidence: None,
                    }),
                    (None, Some("unavailable")) => Err(ExecutionError::ExecutorUnavailable(output)),
                    (None, Some("failed")) => Err(ExecutionError::ExecutionFailed(output)),
                    _ => {
                        return error(
                            "invalid_command",
                            "give status success|failure|cancelled, or error unavailable|failed",
                        );
                    }
                };
                lock(&running.exchange).execution = Some(answer);
                self.advance(&limits)
            }
            _ => error("unknown_command", "unknown command"),
        }
    }

    /// Polls the work until it suspends or finishes, and reports which.
    fn advance(&mut self, limits: &WorkLimits) -> Vec<u8> {
        let running = self.running.as_mut().expect("advance needs a running work");
        let mut cx = Context::from_waker(Waker::noop());
        if let Poll::Ready(report) = running.work.as_mut().poll(&mut cx) {
            let response = respond(&done_json(&report, limits));
            self.running = None;
            self.finished = true;
            return response;
        }
        let pending = lock(&running.exchange).pending.take();
        let (response, waiting) = match pending {
            Some(Pending::Inference { request, id }) => {
                let hex: String = id.0.iter().map(|byte| format!("{byte:02x}")).collect();
                (
                    json!({"status": "need_inference", "request_id": hex, "request": request}),
                    Waiting::Inference,
                )
            }
            Some(Pending::Execution(execution)) => {
                let inputs: BTreeMap<&String, Value> = execution
                    .inputs
                    .iter()
                    .map(|(name, value)| (name, input_json(value)))
                    .collect();
                // Remember which execution the answer belongs to.
                running.execution_id = Some(execution.id.clone());
                (
                    json!({
                        "status": "need_execution",
                        "execution": {
                            "id": execution.id.0,
                            "capability": execution.intent,
                            "inputs": inputs,
                        }
                    }),
                    Waiting::Execution,
                )
            }
            None => {
                self.running = None;
                self.finished = true;
                return error(
                    "stalled",
                    "the work is waiting for neither an inference nor an execution",
                );
            }
        };
        running.waiting_on = Some(waiting);
        running.current = respond(&response);
        running.current.clone()
    }
}
