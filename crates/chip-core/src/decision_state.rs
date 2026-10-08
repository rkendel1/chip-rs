//! The compact state a local decision engine consumes.
//!
//! **Experimental residue; the work loop does not use it.** `CapabilityDecisionState`,
//! `GraphStateToken` and `ImpactState` come from the local-decision research (the tiny model, the
//! Wasm decision module and their corpora). Nothing in `Agent` or `run_work` reads them, and no
//! product command does; only the experiment crates and `chip-cli`'s research commands. They stay
//! here because those crates and their golden files depend on this canonical encoding.
//! (`chip-graph` defines a separate type of the same name.) See `docs/product/crates.md`.
//!
//! Everything a local decision needs is compiled into this value once, and the hot path then
//! operates on it without any repository discovery: no file reads, no Git, no network, no
//! environment, no clock, no model, no execution. It is plain data, so it behaves the same
//! natively and under Wasm.
//!
//! `chip-core` consumes already-computed graph facts. It does not depend on the graph
//! implementation: [`GraphStateToken`] is the minimal opaque representation of the relevant
//! architecture, produced elsewhere (by the graph package, through an adapter) and only
//! consumed here.
//!
//! Two different tokens exist on purpose. The [`GraphStateToken`] describes the relevant
//! architecture. The [`StateToken`] returned by [`CapabilityDecisionState::state_token`]
//! describes the complete local decision context, which also includes evidence and impact.
//!
//! The canonical encoding is explicit and hand-written, not derived from a serializer, so it
//! cannot drift when a library changes. The JSON form is for tests, debugging and interop; it
//! is never the hot-path representation.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use sha2::{Digest, Sha256};

use crate::reasoning::EvidenceState;
use crate::{CapabilityId, InputValue, StateToken};

/// Schema name of both the canonical encoding and the JSON form.
pub const DECISION_SCHEMA: &str = "chip.decision.v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionStateError {
    /// Not `sha256:` followed by 64 lowercase hexadecimal digits.
    InvalidGraphState(String),
    UnknownEvidence(String),
    UnknownImpact(String),
}

impl fmt::Display for DecisionStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecisionStateError::InvalidGraphState(s) => {
                write!(
                    f,
                    "graph state must be sha256:<64 lowercase hex digits>, found {s}"
                )
            }
            DecisionStateError::UnknownEvidence(s) => {
                write!(f, "evidence must be valid, stale or unknown, found {s}")
            }
            DecisionStateError::UnknownImpact(s) => {
                write!(f, "impact must be impacted or unchanged, found {s}")
            }
        }
    }
}

impl Error for DecisionStateError {}

/// Identity of the capability-relevant architecture, as a validated 32-byte digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphStateToken([u8; 32]);

impl GraphStateToken {
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Parses `sha256:<64 lowercase hex digits>`, the form the graph package renders.
    pub fn parse(text: &str) -> Result<Self, DecisionStateError> {
        let invalid = || DecisionStateError::InvalidGraphState(text.to_string());
        let hex = text.strip_prefix("sha256:").ok_or_else(invalid)?;
        if hex.len() != 64 {
            return Err(invalid());
        }
        let mut digest = [0u8; 32];
        for (i, pair) in hex.as_bytes().chunks(2).enumerate() {
            let nibble = |b: u8| match b {
                b'0'..=b'9' => Some(b - b'0'),
                b'a'..=b'f' => Some(b - b'a' + 10),
                _ => None,
            };
            match (nibble(pair[0]), nibble(pair[1])) {
                (Some(hi), Some(lo)) => digest[i] = hi << 4 | lo,
                _ => return Err(invalid()),
            }
        }
        Ok(Self(digest))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl GraphStateToken {
    fn hex(&self) -> [u8; 64] {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut out = [0u8; 64];
        for (i, byte) in self.0.iter().enumerate() {
            out[2 * i] = DIGITS[usize::from(byte >> 4)];
            out[2 * i + 1] = DIGITS[usize::from(byte & 0x0f)];
        }
        out
    }

    /// `sha256:<hex>` in a single allocation.
    fn to_token_string(&self) -> String {
        let mut text = String::with_capacity(71);
        text.push_str("sha256:");
        text.push_str(std::str::from_utf8(&self.hex()).expect("hex digits are ASCII"));
        text
    }
}

impl fmt::Display for GraphStateToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("sha256:")?;
        f.write_str(std::str::from_utf8(&self.hex()).expect("hex digits are ASCII"))
    }
}

/// Whether the capability is currently impacted. The model needs this one fact, not the
/// impact report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ImpactState {
    Impacted,
    Unchanged,
}

impl ImpactState {
    pub fn wire_name(self) -> &'static str {
        match self {
            ImpactState::Impacted => "impacted",
            ImpactState::Unchanged => "unchanged",
        }
    }

    pub fn from_wire_name(name: &str) -> Result<Self, DecisionStateError> {
        match name {
            "impacted" => Ok(ImpactState::Impacted),
            "unchanged" => Ok(ImpactState::Unchanged),
            other => Err(DecisionStateError::UnknownImpact(other.to_string())),
        }
    }

    /// Explicit and permanent: part of the canonical encoding.
    fn code(self) -> u8 {
        match self {
            ImpactState::Impacted => 1,
            ImpactState::Unchanged => 2,
        }
    }
}

impl EvidenceState {
    /// `valid`, `stale` or `unknown`: already established, needs revalidation, never
    /// established.
    pub fn wire_name(self) -> &'static str {
        match self {
            EvidenceState::KnownValid => "valid",
            EvidenceState::KnownStale => "stale",
            EvidenceState::Unknown => "unknown",
        }
    }

    pub fn from_wire_name(name: &str) -> Result<Self, DecisionStateError> {
        match name {
            "valid" => Ok(EvidenceState::KnownValid),
            "stale" => Ok(EvidenceState::KnownStale),
            "unknown" => Ok(EvidenceState::Unknown),
            other => Err(DecisionStateError::UnknownEvidence(other.to_string())),
        }
    }

    /// Explicit and permanent: part of the canonical encoding.
    fn code(self) -> u8 {
        match self {
            EvidenceState::KnownValid => 1,
            EvidenceState::KnownStale => 2,
            EvidenceState::Unknown => 3,
        }
    }
}

/// Everything the local decision is given, and nothing it would have to go and find out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityDecisionState {
    pub capability_id: CapabilityId,
    pub graph_state: GraphStateToken,
    pub evidence_state: EvidenceState,
    pub impact: ImpactState,
    /// The capability's typed inputs. Ordered by name, so insertion order is irrelevant.
    pub inputs: BTreeMap<String, InputValue>,
}

impl CapabilityDecisionState {
    /// Built entirely from the values passed in; touches nothing else.
    pub fn new(
        capability_id: CapabilityId,
        graph_state: GraphStateToken,
        evidence_state: EvidenceState,
        impact: ImpactState,
    ) -> Self {
        Self {
            capability_id,
            graph_state,
            evidence_state,
            impact,
            inputs: BTreeMap::new(),
        }
    }

    pub fn with_input(mut self, name: impl Into<String>, value: InputValue) -> Self {
        self.inputs.insert(name.into(), value);
        self
    }

    /// Streams the canonical encoding to `sink` in fixed order, allocating nothing itself.
    /// Layout (all integers big-endian, every variable-length field length-prefixed):
    ///
    /// `"chip.decision.v1"` 0x00 | u32 len, capability | 32-byte graph state | evidence code |
    /// impact code | u32 input count | per input, by name: u32 len, name, type code
    /// (1 text, 2 integer, 3 bool), value (text: u32 len + bytes; integer: i64; bool: 1 byte)
    fn encode(&self, sink: &mut impl FnMut(&[u8])) {
        let text = |sink: &mut dyn FnMut(&[u8]), s: &str| {
            sink(&(s.len() as u32).to_be_bytes());
            sink(s.as_bytes());
        };
        sink(DECISION_SCHEMA.as_bytes());
        sink(&[0]);
        text(sink, self.capability_id.as_str());
        sink(self.graph_state.as_bytes());
        sink(&[self.evidence_state.code(), self.impact.code()]);
        sink(&(self.inputs.len() as u32).to_be_bytes());
        for (name, value) in &self.inputs {
            text(sink, name);
            match value {
                InputValue::Text(s) => {
                    sink(&[1]);
                    text(sink, s);
                }
                InputValue::Integer(i) => {
                    sink(&[2]);
                    sink(&i.to_be_bytes());
                }
                InputValue::Bool(b) => sink(&[3, u8::from(*b)]),
            }
        }
    }

    /// Appends the canonical bytes to `out`. Reuse one buffer to keep the hot path
    /// allocation-free.
    pub fn write_canonical(&self, out: &mut Vec<u8>) {
        self.encode(&mut |bytes| out.extend_from_slice(bytes));
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(96);
        self.write_canonical(&mut out);
        out
    }

    /// SHA-256 of the canonical bytes, without building them. Allocation-free.
    pub fn state_digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        self.encode(&mut |bytes| hasher.update(bytes));
        hasher.finalize().into()
    }

    /// `sha256:` of the canonical decision state: the identity of the complete local decision
    /// context. Not the same thing as the graph state token it contains.
    pub fn state_token(&self) -> StateToken {
        StateToken::new(GraphStateToken::from_digest(self.state_digest()).to_token_string())
    }

    /// The `chip.decision.v1` JSON form, in fixed field order with no insignificant
    /// whitespace. `inputs` appears only when there are inputs.
    pub fn to_wire_json(&self) -> String {
        let mut out = String::with_capacity(192);
        out.push_str("{\"schema\":\"");
        out.push_str(DECISION_SCHEMA);
        out.push_str("\",\"capability\":");
        json_string(&mut out, self.capability_id.as_str());
        out.push_str(",\"graph_state\":\"");
        out.push_str(&self.graph_state.to_string());
        out.push_str("\",\"evidence\":\"");
        out.push_str(self.evidence_state.wire_name());
        out.push_str("\",\"impact\":\"");
        out.push_str(self.impact.wire_name());
        out.push('"');
        if !self.inputs.is_empty() {
            out.push_str(",\"inputs\":{");
            for (i, (name, value)) in self.inputs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json_string(&mut out, name);
                out.push(':');
                match value {
                    InputValue::Text(s) => json_string(&mut out, s),
                    InputValue::Integer(n) => out.push_str(&n.to_string()),
                    InputValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
                }
            }
            out.push('}');
        }
        out.push('}');
        out
    }
}

fn json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}
