//! The wire format between the backend and the worker: one JSON object each way. The request goes
//! in an environment variable (there is no stdin), the response is the worker's stdout. Nothing
//! here is a capability's meaning; it only carries Chip's own types across the process boundary.

use std::collections::BTreeMap;

use chip_core::{
    CapabilityAvailability, CapabilityDescriptor, CapabilityError, CapabilityId, CapabilityInput,
    ExecutionError, ExecutionId, ExecutionResult, ExecutionStatus, InputValue,
};
use serde_json::{Value, json};

pub const WORKER_ENV: &str = "CHIP_CAPABILITY_REQUEST";
pub const WORKER_SUBCOMMAND: &str = "capability-exec";
const VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Info,
    Capabilities,
    Availability {
        capability: String,
    },
    Validate {
        capability: String,
        inputs: BTreeMap<String, InputValue>,
    },
    Execute {
        execution_id: String,
        capability: String,
        inputs: BTreeMap<String, InputValue>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    /// Where the project is, as the environment sees it. Used for the observation invariants only.
    pub root: String,
    pub verifier_version: Option<String>,
    pub verifier_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Info(Info),
    Capabilities(Vec<CapabilityDescriptor>),
    Availability(CapabilityAvailability),
    Validated(Result<(), CapabilityError>),
    Executed(Result<ExecutionResult, ExecutionError>),
}

fn inputs_json(inputs: &BTreeMap<String, InputValue>) -> Value {
    Value::Object(
        inputs
            .iter()
            .map(|(k, v)| {
                let v = match v {
                    InputValue::Text(s) => json!({"text": s}),
                    InputValue::Integer(n) => json!({"integer": n}),
                    InputValue::Bool(b) => json!({"bool": b}),
                };
                (k.clone(), v)
            })
            .collect(),
    )
}

fn inputs_from(value: &Value) -> Result<BTreeMap<String, InputValue>, String> {
    let object = value.as_object().ok_or("inputs must be an object")?;
    object
        .iter()
        .map(|(name, v)| {
            let v = v
                .as_object()
                .filter(|o| o.len() == 1)
                .ok_or("malformed input")?;
            let (kind, v) = v.iter().next().unwrap();
            let value = match kind.as_str() {
                "text" => InputValue::Text(v.as_str().ok_or("malformed text input")?.to_string()),
                "integer" => InputValue::Integer(v.as_i64().ok_or("malformed integer input")?),
                "bool" => InputValue::Bool(v.as_bool().ok_or("malformed bool input")?),
                _ => return Err("unknown input type".to_string()),
            };
            Ok((name.clone(), value))
        })
        .collect()
}

fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {key}"))
}

fn optional_text(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

impl Request {
    pub fn encode(&self) -> String {
        let body = match self {
            Request::Info => json!({"op": "info"}),
            Request::Capabilities => json!({"op": "capabilities"}),
            Request::Availability { capability } => {
                json!({"op": "availability", "capability": capability})
            }
            Request::Validate { capability, inputs } => {
                json!({"op": "validate", "capability": capability, "inputs": inputs_json(inputs)})
            }
            Request::Execute {
                execution_id,
                capability,
                inputs,
            } => json!({
                "op": "execute", "execution_id": execution_id,
                "capability": capability, "inputs": inputs_json(inputs),
            }),
        };
        let mut body = body;
        body["v"] = json!(VERSION);
        body.to_string()
    }

    pub fn decode(encoded: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(encoded).map_err(|e| format!("not JSON: {e}"))?;
        if v.get("v").and_then(Value::as_u64) != Some(VERSION) {
            return Err("unsupported protocol version".into());
        }
        match text(&v, "op")? {
            "info" => Ok(Request::Info),
            "capabilities" => Ok(Request::Capabilities),
            "availability" => Ok(Request::Availability {
                capability: text(&v, "capability")?.to_string(),
            }),
            "validate" => Ok(Request::Validate {
                capability: text(&v, "capability")?.to_string(),
                inputs: inputs_from(v.get("inputs").unwrap_or(&json!({})))?,
            }),
            "execute" => Ok(Request::Execute {
                execution_id: text(&v, "execution_id")?.to_string(),
                capability: text(&v, "capability")?.to_string(),
                inputs: inputs_from(v.get("inputs").unwrap_or(&json!({})))?,
            }),
            other => Err(format!("unknown operation `{other}`")),
        }
    }
}

fn descriptor_json(d: &CapabilityDescriptor) -> Value {
    json!({
        "id": d.id.as_str(), "name": d.name, "description": d.description,
        "version": d.version, "max_input_bytes": d.max_input_bytes,
        "reuse_evidence": d.reuse_evidence,
        "inputs": d.inputs.iter().map(|i| json!({
            "name": i.name, "description": i.description, "required": i.required,
        })).collect::<Vec<_>>(),
    })
}

fn descriptor_from(v: &Value) -> Result<CapabilityDescriptor, String> {
    let id = CapabilityId::new(text(v, "id")?).map_err(|e| format!("{e:?}"))?;
    let mut d = CapabilityDescriptor::new(id, text(v, "name")?, text(v, "description")?);
    d.version = optional_text(v, "version");
    d.max_input_bytes = v
        .get("max_input_bytes")
        .and_then(Value::as_u64)
        .map(|n| n as usize);
    d.reuse_evidence = v
        .get("reuse_evidence")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    for i in v
        .get("inputs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        d.inputs.push(CapabilityInput {
            name: text(i, "name")?.to_string(),
            description: text(i, "description")?.to_string(),
            required: i.get("required").and_then(Value::as_bool).unwrap_or(false),
        });
    }
    Ok(d)
}

fn capability_error_json(e: &CapabilityError) -> Value {
    let (kind, message) = match e {
        CapabilityError::InvalidId(m) => ("invalid_id", m),
        CapabilityError::Unknown(m) => ("unknown", m),
        CapabilityError::Unavailable(m) => ("unavailable", m),
        CapabilityError::InvalidInput(m) => ("invalid_input", m),
    };
    json!({"kind": kind, "message": message})
}

fn capability_error_from(v: &Value) -> Result<CapabilityError, String> {
    let m = text(v, "message")?.to_string();
    Ok(match text(v, "kind")? {
        "invalid_id" => CapabilityError::InvalidId(m),
        "unknown" => CapabilityError::Unknown(m),
        "unavailable" => CapabilityError::Unavailable(m),
        "invalid_input" => CapabilityError::InvalidInput(m),
        other => return Err(format!("unknown error kind `{other}`")),
    })
}

fn execution_error_json(e: &ExecutionError) -> Value {
    let (kind, message) = match e {
        ExecutionError::InvalidRequest(m) => ("invalid_request", m.as_str()),
        ExecutionError::ExecutorUnavailable(m) => ("executor_unavailable", m.as_str()),
        ExecutionError::ExecutionFailed(m) => ("execution_failed", m.as_str()),
        ExecutionError::Cancelled => ("cancelled", ""),
    };
    json!({"kind": kind, "message": message})
}

fn execution_error_from(v: &Value) -> Result<ExecutionError, String> {
    let m = text(v, "message")?.to_string();
    Ok(match text(v, "kind")? {
        "invalid_request" => ExecutionError::InvalidRequest(m),
        "executor_unavailable" => ExecutionError::ExecutorUnavailable(m),
        "execution_failed" => ExecutionError::ExecutionFailed(m),
        "cancelled" => ExecutionError::Cancelled,
        other => return Err(format!("unknown error kind `{other}`")),
    })
}

impl Response {
    pub fn encode(&self) -> String {
        let mut body = match self {
            Response::Info(i) => json!({"op": "info", "root": i.root,
                "verifier_version": i.verifier_version, "verifier_error": i.verifier_error}),
            Response::Capabilities(ds) => json!({"op": "capabilities",
                "capabilities": ds.iter().map(descriptor_json).collect::<Vec<_>>()}),
            Response::Availability(a) => {
                let (state, reason) = match a {
                    CapabilityAvailability::Available => ("available", None),
                    CapabilityAvailability::Unavailable(r) => ("unavailable", Some(r)),
                    CapabilityAvailability::Misconfigured(r) => ("misconfigured", Some(r)),
                };
                json!({"op": "availability", "state": state, "reason": reason})
            }
            Response::Validated(r) => match r {
                Ok(()) => json!({"op": "validate", "valid": true}),
                Err(e) => {
                    json!({"op": "validate", "valid": false, "error": capability_error_json(e)})
                }
            },
            Response::Executed(r) => match r {
                Ok(result) => json!({"op": "execute", "result": {
                    "execution_id": result.id.to_string(),
                    "status": match result.status {
                        ExecutionStatus::Success => "success",
                        ExecutionStatus::Failure => "failure",
                        ExecutionStatus::Cancelled => "cancelled",
                    },
                    "output": result.output, "receipt_id": result.receipt_id,
                }}),
                Err(e) => json!({"op": "execute", "error": execution_error_json(e)}),
            },
        };
        body["v"] = json!(VERSION);
        body.to_string()
    }

    pub fn decode(encoded: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(encoded.trim())
            .map_err(|e| format!("the worker's answer is not JSON: {e}"))?;
        if v.get("v").and_then(Value::as_u64) != Some(VERSION) {
            return Err("unsupported protocol version".into());
        }
        match text(&v, "op")? {
            "info" => Ok(Response::Info(Info {
                root: text(&v, "root")?.to_string(),
                verifier_version: optional_text(&v, "verifier_version"),
                verifier_error: optional_text(&v, "verifier_error"),
            })),
            "capabilities" => Ok(Response::Capabilities(
                v.get("capabilities")
                    .and_then(Value::as_array)
                    .ok_or("missing capabilities")?
                    .iter()
                    .map(descriptor_from)
                    .collect::<Result<_, _>>()?,
            )),
            "availability" => {
                let reason = optional_text(&v, "reason").unwrap_or_default();
                Ok(Response::Availability(match text(&v, "state")? {
                    "available" => CapabilityAvailability::Available,
                    "unavailable" => CapabilityAvailability::Unavailable(reason),
                    "misconfigured" => CapabilityAvailability::Misconfigured(reason),
                    other => return Err(format!("unknown availability `{other}`")),
                }))
            }
            "validate" => Ok(Response::Validated(
                if v.get("valid").and_then(Value::as_bool) == Some(true) {
                    Ok(())
                } else {
                    Err(capability_error_from(
                        v.get("error").ok_or("missing error")?,
                    )?)
                },
            )),
            "execute" => {
                if let Some(e) = v.get("error") {
                    return Ok(Response::Executed(Err(execution_error_from(e)?)));
                }
                let r = v.get("result").ok_or("missing result")?;
                let status = match text(r, "status")? {
                    "success" => ExecutionStatus::Success,
                    "failure" => ExecutionStatus::Failure,
                    "cancelled" => ExecutionStatus::Cancelled,
                    other => return Err(format!("unknown status `{other}`")),
                };
                Ok(Response::Executed(Ok(ExecutionResult {
                    id: ExecutionId::new(text(r, "execution_id")?),
                    status,
                    output: text(r, "output")?.to_string(),
                    receipt_id: optional_text(r, "receipt_id"),
                })))
            }
            other => Err(format!("unknown operation `{other}`")),
        }
    }
}
