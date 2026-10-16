//! Reading the execution report a host hands back.
//!
//! When FX runs somewhere else (the WebAssembly session in `fx-host`, driven by a Node host
//! over Easy-LLM), Chip receives the report as the JSON document `fx_core::encode_report`
//! wrote. This module reads it with FX's own decoder (`fx_core::decode_outcome`) and applies
//! the same mapping as the native path, so there is one meaning for an outcome.

use chip_inference::{FxError, ModelResponse};
use fx_core::{CONTRACT_VERSION, Disposition, Limits, RequestId, decode_outcome};
use serde_json::{Value, json};

use crate::mapping::from_outcome;

/// The disposition names `fx_core::encode_report` writes.
pub fn disposition_from_name(name: &str) -> Option<Disposition> {
    Some(match name {
        "complete" => Disposition::Complete,
        "tool_proposals" => Disposition::ToolProposals,
        "length_limited" => Disposition::LengthLimited,
        "provider_failure" => Disposition::ProviderFailure,
        "invalid_completion" => Disposition::InvalidCompletion,
        _ => return None,
    })
}

fn invalid(reason: &str) -> FxError {
    FxError::InvalidResponse(format!("the FX report is not readable: {reason}"))
}

/// Maps an FX report document to a Chip response or failure. Fails closed: a report that is
/// not exactly the documented shape, version or disposition is an error, never a guess.
pub fn from_report_json(json: &str, request_id: RequestId) -> Result<ModelResponse, FxError> {
    let value: Value = serde_json::from_str(json).map_err(|_| invalid("not JSON"))?;
    if value.get("v").and_then(Value::as_u64) != Some(u64::from(CONTRACT_VERSION)) {
        return Err(invalid("unsupported contract version"));
    }
    let report = value.get("report").ok_or_else(|| invalid("no report"))?;
    let outcome = report.get("outcome").ok_or_else(|| invalid("no outcome"))?;
    let disposition = match report.get("disposition") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) => {
            Some(disposition_from_name(name).ok_or_else(|| invalid("unknown disposition"))?)
        }
        Some(_) => return Err(invalid("disposition has the wrong type")),
    };
    let envelope = json!({ "v": CONTRACT_VERSION, "outcome": outcome }).to_string();
    let outcome = decode_outcome(envelope.as_bytes(), &Limits::default())
        .map_err(|_| invalid("the outcome does not decode"))?;
    from_outcome(&outcome, disposition, request_id)
}
