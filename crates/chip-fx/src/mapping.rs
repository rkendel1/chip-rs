//! Pure translation between Chip's model port and the authoritative FX contract.

use std::time::Duration;

use chip_inference::{
    Delivery, FxError, InferenceFailure, MessageRole, ModelRequest, ModelResponse, Usage,
};
use fx_core::{
    AttemptBudget, CacheHint, Correlation, DeliveryCertainty, Disposition, ErrorKind, Message,
    ModelRef, Outcome, Part, Request, RequestId, Role,
};

/// What to do with the sampling options Chip's request carries (`max_tokens`,
/// `temperature`). The authoritative FX path cannot transport generation options through the
/// Easy-LLM adapters (the provider conformance manifest lists `generation_options` as
/// unsupported for every provider), so this is an explicit choice, never a default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptionsPolicy {
    /// Fail closed: `temperature` is refused here, and `max_tokens` is sent to FX, which
    /// refuses it before any provider call when its port cannot carry it.
    Refuse,
    /// Send neither and say so in [`crate::ProviderStats::options_dropped`]. The provider's
    /// own defaults apply. This differs from the removed legacy HTTP provider,
    /// which sent the values (see `docs/product/authoritative-fx.md`, D2).
    DropUnsupported,
    /// Send both to FX: `max_tokens` as the output cap and `temperature` as thousandths. A port
    /// that cannot carry them refuses the request before any provider call. A temperature
    /// outside 0.0 to 2.0, or not a number, is an invalid request.
    Carry,
}

/// Deterministic request ids: an 8-byte seed the host chooses, then a counter. Chip has no
/// random source; uniqueness across processes is the seed's job.
#[derive(Clone, Debug)]
pub struct RequestIds {
    seed: [u8; 8],
    next: u64,
}

impl RequestIds {
    /// Ids from `seed`, counting from zero.
    pub fn new(seed: [u8; 8]) -> RequestIds {
        RequestIds { seed, next: 0 }
    }

    /// The next id.
    pub fn issue(&mut self) -> RequestId {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&self.seed);
        bytes[8..].copy_from_slice(&self.next.to_be_bytes());
        self.next += 1;
        RequestId(bytes)
    }
}

fn hex(id: &RequestId) -> String {
    id.0.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn role(role: &MessageRole) -> Role {
    match role {
        MessageRole::System => Role::System,
        MessageRole::User => Role::User,
        MessageRole::Assistant => Role::Assistant,
    }
}

/// Builds the FX request for a Chip model request. Text only: Chip's decisions are text.
///
/// FX validates the result before any provider call; this function only translates.
pub fn to_fx_request(
    request: &ModelRequest,
    id: RequestId,
    policy: OptionsPolicy,
    budget: Option<Duration>,
) -> Result<Request, FxError> {
    let mut fx_request = Request {
        model: ModelRef(request.model.to_string()),
        messages: request
            .messages
            .iter()
            .map(|message| Message {
                role: role(&message.role),
                parts: vec![Part::Text(message.content.clone())],
                cache: CacheHint::default(),
            })
            .collect(),
        tools: Vec::new(),
        tool_choice: fx_core::ToolChoice::default(),
        options: fx_core::GenerationOptions::default(),
        correlation: Correlation {
            request_id: id,
            affinity_key: None,
        },
        budget: budget.map(AttemptBudget),
    };
    if policy == OptionsPolicy::Carry {
        fx_request.options.max_output_tokens = request.max_tokens;
        fx_request.options.temperature_milli =
            request.temperature.map(temperature_milli).transpose()?;
    }
    if policy == OptionsPolicy::Refuse {
        if request.temperature.is_some() {
            return Err(FxError::InvalidRequest(
                "temperature cannot be carried by the authoritative FX path".to_owned(),
            ));
        }
        fx_request.options.max_output_tokens = request.max_tokens;
    }
    Ok(fx_request)
}

/// A temperature in thousandths. Exact for the values Chip uses (`0.0` and `0.7` become 0 and 700).
fn temperature_milli(temperature: f32) -> Result<u16, FxError> {
    let milli = (f64::from(temperature) * 1000.0).round();
    if !milli.is_finite()
        || !(0.0..=f64::from(fx_core::GenerationOptions::MAX_TEMPERATURE_MILLI)).contains(&milli)
    {
        return Err(FxError::InvalidRequest(format!(
            "temperature {temperature} is outside 0.0 to 2.0"
        )));
    }
    Ok(milli as u16)
}

/// The stable label of an FX failure kind. Exhaustive on purpose: a new FX kind is a
/// compile error here, not a silently unlabeled failure.
pub fn kind_label(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::InvalidRequest => "invalid_request",
        ErrorKind::CapabilityUnavailable => "capability_unavailable",
        ErrorKind::Unauthenticated => "unauthenticated",
        ErrorKind::Forbidden => "forbidden",
        ErrorKind::ModelUnavailable => "model_unavailable",
        ErrorKind::RateLimited => "rate_limited",
        ErrorKind::ProviderUnavailable => "provider_unavailable",
        ErrorKind::Transport => "transport",
        ErrorKind::SystemResumed => "system_resumed",
        ErrorKind::Timeout => "timeout",
        ErrorKind::Cancelled => "cancelled",
        ErrorKind::ProtocolViolation => "protocol_violation",
        ErrorKind::ResourceLimit => "resource_limit",
        ErrorKind::Internal => "internal",
    }
}

fn delivery(certainty: DeliveryCertainty) -> Delivery {
    match certainty {
        DeliveryCertainty::DefinitelyUnsent => Delivery::DefinitelyUnsent,
        DeliveryCertainty::PossiblySent => Delivery::PossiblySent,
        DeliveryCertainty::ConfirmedResponse => Delivery::ConfirmedResponse,
    }
}

fn failure(
    kind: &str,
    delivery: Delivery,
    detail: Option<String>,
    partial: Option<String>,
) -> FxError {
    FxError::Inference(Box::new(InferenceFailure {
        kind: kind.to_owned(),
        delivery,
        status: None,
        retry_after_ms: None,
        detail,
        partial_output: partial,
    }))
}

fn clamp(value: Option<u64>) -> u32 {
    value.map_or(0, |value| u32::try_from(value).unwrap_or(u32::MAX))
}

/// Turns one FX terminal outcome into a Chip response or a failure.
///
/// A response only comes from `Completed` classified [`Disposition::Complete`]. Everything
/// else keeps what FX knew: the failure kind, the delivery certainty, a status and
/// retry-after as diagnostics, and any partial output. Nothing here retries or repairs.
pub fn from_outcome(
    outcome: &Outcome,
    disposition: Option<Disposition>,
    request_id: RequestId,
) -> Result<ModelResponse, FxError> {
    match outcome {
        Outcome::Completed(completion) => {
            let delivery = Delivery::ConfirmedResponse;
            match disposition {
                Some(Disposition::Complete) if !completion.content_truncated => {
                    let usage = completion.usage.as_ref();
                    let input = clamp(usage.and_then(|usage| usage.input_tokens));
                    let output = clamp(usage.and_then(|usage| usage.output_tokens));
                    let id = completion
                        .metadata
                        .generation_id
                        .clone()
                        .unwrap_or_else(|| hex(&request_id));
                    Ok(ModelResponse::new(
                        id,
                        completion.content.clone(),
                        Usage::new(input, output),
                    ))
                }
                Some(Disposition::Complete) => Err(failure(
                    "content_truncated",
                    delivery,
                    Some("the reply exceeded the executor's content bound".to_owned()),
                    Some(completion.content.clone()),
                )),
                Some(Disposition::LengthLimited) => Err(failure(
                    "length_limited",
                    delivery,
                    Some("the provider stopped at its output limit".to_owned()),
                    Some(completion.content.clone()),
                )),
                Some(Disposition::ToolProposals) => Err(failure(
                    "tool_proposals_unexpected",
                    delivery,
                    Some("a tool call was proposed to a request that offered no tools".to_owned()),
                    None,
                )),
                Some(Disposition::ProviderFailure) => Err(failure(
                    "provider_failure",
                    delivery,
                    Some("the provider reported an in-band error or a content filter".to_owned()),
                    None,
                )),
                Some(Disposition::InvalidCompletion) | None => Err(failure(
                    "invalid_completion",
                    delivery,
                    Some("the completion contradicts its finish reason".to_owned()),
                    None,
                )),
            }
        }
        Outcome::Failed(f) => Err(FxError::Inference(Box::new(InferenceFailure {
            kind: kind_label(f.kind).to_owned(),
            delivery: delivery(f.delivery),
            status: f.status_hint,
            retry_after_ms: f
                .retry_after
                .map(|wait| u64::try_from(wait.as_millis()).unwrap_or(u64::MAX)),
            detail: f.detail.as_ref().map(|detail| detail.as_str().to_owned()),
            partial_output: f.partial.as_ref().map(|partial| partial.content.clone()),
        }))),
        Outcome::Cancelled(c) => Err(failure(
            "cancelled",
            delivery(c.delivery),
            None,
            c.partial.as_ref().map(|partial| partial.content.clone()),
        )),
    }
}
