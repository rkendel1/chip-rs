//! The smallest contract between a model's text and a [`WorkDecision`] (`chip.work-decision.v1`).
//!
//! The model produces *data*: one JSON object naming a decision. Chip reads it strictly, validates
//! it, and only then does anything follow; the model never produces an instruction that is run.
//! A capability request is still just a request: it passes through the same validation, execution
//! and observation path as any other, and Compute is never told anything the model wrote except a
//! declared capability id and typed input values.
//!
//! ```text
//! (each object may also carry "schema":"chip.work-decision.v1")
//! {"decision":"request_capability","capability":"<id>","inputs":{"<name>":<string|integer|boolean>}}
//! {"decision":"complete","summary":"<text>"}
//! {"decision":"escalate","reason":"<text>"}
//! {"decision":"block","reason":"<text>"}
//! ```
//!
//! Strict by design: anything else (prose around the object, an unknown decision, a missing or an
//! extra field, a wrong type, an undeclared or malformed capability, an oversized reply) is an
//! error. There is no repair, no retry and no second model call. A single Markdown code fence
//! around the whole reply is tolerated, because models add one habitually; nothing else is.

use std::collections::BTreeMap;

use fx_core::ModelResponse;

use crate::{
    Capability, CapabilityAvailability, CapabilityError, CapabilityId, CapabilityRequest,
    DecisionError, ExecutionId, InputValue, WorkDecision, WorkDecisionBoundary,
};

pub const WORK_DECISION_SCHEMA: &str = "chip.work-decision.v1";

/// Largest reply the boundary will read.
const MAX_REPLY_BYTES: usize = 4096;
const MAX_TEXT_BYTES: usize = 1024;
const MAX_INPUTS: usize = 16;

/// Interprets a model reply under the `chip.work-decision.v1` contract.
#[derive(Debug, Clone, Copy, Default)]
pub struct ModelDecisionBoundary;

fn invalid(message: impl Into<String>) -> DecisionError {
    DecisionError::InvalidDecision(message.into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Json {
    Str(String),
    Int(i64),
    Bool(bool),
    Object(Vec<(String, Json)>),
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Parser<'a> {
    fn skip_space(&mut self) {
        while self
            .bytes
            .get(self.at)
            .is_some_and(|b| b.is_ascii_whitespace())
        {
            self.at += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.bytes.get(self.at) == Some(&byte) {
            self.at += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at byte {}", byte as char, self.at))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        self.skip_space();
        match self.bytes.get(self.at) {
            Some(b'{') if depth < 2 => self.object(depth),
            Some(b'{') => Err("objects nested too deeply".to_string()),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.word("true", Json::Bool(true)),
            Some(b'f') => self.word("false", Json::Bool(false)),
            Some(b'-' | b'0'..=b'9') => self.integer(),
            Some(b'[') => Err("arrays are not part of the contract".to_string()),
            Some(b'n') => Err("null is not part of the contract".to_string()),
            Some(_) => Err(format!("unexpected character at byte {}", self.at)),
            None => Err("unexpected end of reply".to_string()),
        }
    }

    fn word(&mut self, word: &str, value: Json) -> Result<Json, String> {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            Ok(value)
        } else {
            Err(format!("unexpected word at byte {}", self.at))
        }
    }

    fn integer(&mut self) -> Result<Json, String> {
        let start = self.at;
        if self.bytes.get(self.at) == Some(&b'-') {
            self.at += 1;
        }
        let digits = self.at;
        while self.bytes.get(self.at).is_some_and(u8::is_ascii_digit) {
            self.at += 1;
        }
        if self.at == digits {
            return Err("a number needs digits".to_string());
        }
        if matches!(self.bytes.get(self.at), Some(b'.' | b'e' | b'E')) {
            return Err("only whole numbers are part of the contract".to_string());
        }
        std::str::from_utf8(&self.bytes[start..self.at])
            .ok()
            .and_then(|t| t.parse::<i64>().ok())
            .map(Json::Int)
            .ok_or_else(|| "number out of range".to_string())
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let Some(&b) = self.bytes.get(self.at) else {
                return Err("unterminated string".to_string());
            };
            self.at += 1;
            match b {
                b'"' => return Ok(out),
                b'\\' => {
                    let Some(&e) = self.bytes.get(self.at) else {
                        return Err("unterminated escape".to_string());
                    };
                    self.at += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex = self
                                .bytes
                                .get(self.at..self.at + 4)
                                .ok_or("short \\u escape")?;
                            let code = std::str::from_utf8(hex)
                                .ok()
                                .and_then(|h| u32::from_str_radix(h, 16).ok())
                                .ok_or("bad \\u escape")?;
                            self.at += 4;
                            out.push(
                                char::from_u32(code).ok_or("unpaired surrogate in a \\u escape")?,
                            );
                        }
                        _ => return Err("unknown escape".to_string()),
                    }
                }
                b if b < 0x20 => return Err("a control character inside a string".to_string()),
                _ => {
                    // Copy one UTF-8 scalar whole.
                    let start = self.at - 1;
                    let width = match b {
                        0x00..=0x7f => 1,
                        0xc0..=0xdf => 2,
                        0xe0..=0xef => 3,
                        _ => 4,
                    };
                    let end = start + width;
                    let slice = self.bytes.get(start..end).ok_or("truncated UTF-8")?;
                    out.push_str(std::str::from_utf8(slice).map_err(|_| "invalid UTF-8")?);
                    self.at = end;
                }
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, String> {
        self.expect(b'{')?;
        let mut fields: Vec<(String, Json)> = Vec::new();
        self.skip_space();
        if self.bytes.get(self.at) == Some(&b'}') {
            self.at += 1;
            return Ok(Json::Object(fields));
        }
        loop {
            self.skip_space();
            let key = self.string()?;
            if fields.iter().any(|(k, _)| *k == key) {
                return Err(format!("duplicate field \"{key}\""));
            }
            self.skip_space();
            self.expect(b':')?;
            let value = self.value(depth + 1)?;
            fields.push((key, value));
            self.skip_space();
            match self.bytes.get(self.at) {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(Json::Object(fields));
                }
                _ => return Err(format!("expected ',' or '}}' at byte {}", self.at)),
            }
        }
    }
}

/// The reply as one object: trimmed, optionally unwrapped from a single code fence, and nothing
/// after the closing brace.
fn parse_reply(output: &str) -> Result<Vec<(String, Json)>, DecisionError> {
    if output.len() > MAX_REPLY_BYTES {
        return Err(invalid(format!(
            "the reply is {} bytes; the limit is {MAX_REPLY_BYTES}",
            output.len()
        )));
    }
    let mut text = output.trim();
    if let Some(rest) = text.strip_prefix("```") {
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        let Some(body) = rest.trim_end().strip_suffix("```") else {
            return Err(invalid("an unclosed code fence"));
        };
        text = body.trim();
    }
    let mut parser = Parser {
        bytes: text.as_bytes(),
        at: 0,
    };
    let value = parser
        .value(0)
        .map_err(|e| invalid(format!("not valid JSON: {e}")))?;
    parser.skip_space();
    if parser.at != text.len() {
        return Err(invalid("text after the JSON object"));
    }
    match value {
        Json::Object(fields) => Ok(fields),
        _ => Err(invalid("the reply must be a JSON object")),
    }
}

fn take_string(fields: &mut Vec<(String, Json)>, key: &str) -> Result<String, DecisionError> {
    let position = fields
        .iter()
        .position(|(k, _)| k == key)
        .ok_or_else(|| invalid(format!("missing required field \"{key}\"")))?;
    match fields.remove(position).1 {
        Json::Str(s) if s.len() <= MAX_TEXT_BYTES => Ok(s),
        Json::Str(_) => Err(invalid(format!("\"{key}\" is too long"))),
        _ => Err(invalid(format!("\"{key}\" must be a string"))),
    }
}

fn reject_extras(fields: &[(String, Json)]) -> Result<(), DecisionError> {
    match fields.first() {
        Some((k, _)) => Err(invalid(format!("unexpected field \"{k}\""))),
        None => Ok(()),
    }
}

/// Keeps only characters that cannot mean anything to an executor.
fn identifier(provider_id: &str) -> String {
    let cleaned: String = provider_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(32)
        .collect();
    if cleaned.is_empty() {
        "request".to_string()
    } else {
        cleaned
    }
}

impl WorkDecisionBoundary for ModelDecisionBoundary {
    fn interpret(
        &self,
        response: &ModelResponse,
        capabilities: &[Capability],
    ) -> Result<WorkDecision, DecisionError> {
        let mut fields = parse_reply(&response.output)?;
        // The schema marker is optional (models echo it when told the schema) but, if present,
        // it must name this contract.
        if fields.iter().any(|(k, _)| k == "schema") {
            let schema = take_string(&mut fields, "schema")?;
            if schema != WORK_DECISION_SCHEMA {
                return Err(invalid(format!("unsupported schema \"{schema}\"")));
            }
        }
        let decision = take_string(&mut fields, "decision")?;
        match decision.as_str() {
            "request_capability" => {
                let capability = take_string(&mut fields, "capability")?;
                let inputs = match fields.iter().position(|(k, _)| k == "inputs") {
                    None => BTreeMap::new(),
                    Some(i) => match fields.remove(i).1 {
                        Json::Object(entries) if entries.len() <= MAX_INPUTS => entries
                            .into_iter()
                            .map(|(name, value)| match value {
                                Json::Str(s) if s.len() <= MAX_TEXT_BYTES => {
                                    Ok((name, InputValue::Text(s)))
                                }
                                Json::Int(n) => Ok((name, InputValue::Integer(n))),
                                Json::Bool(b) => Ok((name, InputValue::Bool(b))),
                                _ => Err(invalid(format!(
                                    "input \"{name}\" must be a string, an integer or a boolean"
                                ))),
                            })
                            .collect::<Result<BTreeMap<_, _>, _>>()?,
                        Json::Object(_) => return Err(invalid("too many inputs")),
                        _ => return Err(invalid("\"inputs\" must be an object")),
                    },
                };
                reject_extras(&fields)?;
                let id = CapabilityId::new(capability)?;
                // The model may only ask for what has been declared and can be used.
                match capabilities.iter().find(|c| c.descriptor.id == id) {
                    None => return Err(CapabilityError::Unknown(id.to_string()).into()),
                    Some(c) if c.availability != CapabilityAvailability::Available => {
                        return Err(
                            CapabilityError::Unavailable(format!("{id} is not available")).into(),
                        );
                    }
                    Some(_) => {}
                }
                Ok(WorkDecision::RequestCapability(CapabilityRequest {
                    // Chip names the execution; the model's text never becomes an identifier.
                    execution_id: ExecutionId::new(format!("model-{}", identifier(&response.id))),
                    capability_id: id,
                    inputs,
                }))
            }
            "complete" => {
                let summary = take_string(&mut fields, "summary")?;
                reject_extras(&fields)?;
                Ok(WorkDecision::Complete { summary })
            }
            "escalate" => {
                let reason = take_string(&mut fields, "reason")?;
                reject_extras(&fields)?;
                Ok(WorkDecision::Escalate { reason })
            }
            "block" => {
                let reason = take_string(&mut fields, "reason")?;
                reject_extras(&fields)?;
                Ok(WorkDecision::Block { reason })
            }
            other => Err(invalid(format!(
                "unknown decision \"{}\"",
                other.chars().take(40).collect::<String>()
            ))),
        }
    }

    fn question(&self, capabilities: &[Capability]) -> String {
        let mut available: Vec<String> = capabilities
            .iter()
            .filter(|c| c.availability == CapabilityAvailability::Available)
            .map(|c| {
                let inputs: Vec<&str> = c
                    .descriptor
                    .inputs
                    .iter()
                    .map(|i| i.name.as_str())
                    .collect();
                if inputs.is_empty() {
                    c.descriptor.id.to_string()
                } else {
                    format!("{} (inputs: {})", c.descriptor.id, inputs.join(", "))
                }
            })
            .collect();
        available.sort();
        format!(
            "Decide the next step. Reply with exactly one JSON object and nothing else; it is read as data and never run. Every object carries \"schema\":\"{WORK_DECISION_SCHEMA}\" (optional) and one of: \
{{\"decision\":\"request_capability\",\"capability\":\"<id>\",\"inputs\":{{\"<name>\":<string|integer|boolean>}}}} (inputs optional), \
{{\"decision\":\"complete\",\"summary\":\"<text>\"}}, {{\"decision\":\"escalate\",\"reason\":\"<text>\"}} or {{\"decision\":\"block\",\"reason\":\"<text>\"}}. \
Available capabilities: {}.",
            if available.is_empty() {
                "none".to_string()
            } else {
                available.join("; ")
            }
        )
    }
}
