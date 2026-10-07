//! Local evidence: observations of real executions that an agent has received,
//! kept in memory for the lifetime of that agent so an identical operation need
//! not be repeated.
//!
//! This is not memory. An entry can only come from an `Observation` of an actual
//! execution result, and it is keyed by the operation (capability plus
//! normalized inputs), never by the fact that something was requested.
//! Lookup is deterministic and involves no model.
//!
//! Seam for later work: the agent's order of preference is
//! local deterministic checks (this module), then an optional small local
//! reasoner for cheap judgments (is this evidence still relevant, does it satisfy
//! the condition, is escalation needed), then escalation to the model. Only the
//! first stage exists today.
//!
//! Validity: evidence can carry the `StateToken` under which it was established.
//! The token is explicit data supplied by the caller; Chip only stores and
//! compares it for equality and never inspects the environment or interprets it.
//! The capability owns what its token means, so tokens for different capabilities
//! are unrelated. Evidence is stale only when its stored token differs from the
//! current one: no clocks, counters or heuristics are involved.
//! The Compute adapter exposes no state identity yet, so Compute state
//! invalidation is not modeled; real-Compute evidence is recorded without a token.
//!
//! Known limit: inputs are part of the key, but are not yet forwarded to
//! executions, so distinct inputs are conservatively treated as distinct operations.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::{CapabilityId, CapabilityRequest, InputValue, Observation};

/// Identity of reusable evidence: capability plus inputs in a normalized
/// (sorted) order. The execution id is deliberately not part of it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EvidenceKey {
    pub capability_id: CapabilityId,
    pub inputs: BTreeMap<String, InputValue>,
}

impl EvidenceKey {
    pub fn from_request(request: &CapabilityRequest) -> Self {
        Self {
            capability_id: request.capability_id.clone(),
            inputs: request.inputs.clone(),
        }
    }
}

/// Opaque identity of the state a piece of evidence was established under.
/// Compared for equality only.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct StateToken(String);

impl StateToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `NotFound` means no knowledge. `Stale` means evidence exists but was
/// established under different state, so it is not current. A found failure is
/// knowledge that it failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceLookup {
    Found(Observation),
    Stale,
    NotFound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EvidenceStats {
    pub lookups: usize,
    pub hits: usize,
    pub misses: usize,
    pub stale: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceError {
    /// The observation is not of the execution this request produced.
    ExecutionMismatch(String),
}

impl fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExecutionMismatch(message) => write!(f, "evidence mismatch: {message}"),
        }
    }
}

impl Error for EvidenceError {}

/// The observation and the state it was established under (if any).
#[derive(Debug)]
struct StoredEvidence {
    observation: Observation,
    state: Option<StateToken>,
}

#[derive(Debug, Default)]
pub(crate) struct EvidenceStore {
    entries: BTreeMap<EvidenceKey, StoredEvidence>,
    stats: EvidenceStats,
}

impl EvidenceStore {
    /// Replaces any earlier evidence for the same operation.
    pub(crate) fn record(
        &mut self,
        key: EvidenceKey,
        observation: Observation,
        state: Option<StateToken>,
    ) {
        self.entries
            .insert(key, StoredEvidence { observation, state });
    }

    /// Evidence is valid only when its stored state equals `current`; evidence
    /// recorded without state matches only a lookup without state. Looking up
    /// never changes or removes an entry.
    pub(crate) fn lookup(
        &mut self,
        key: &EvidenceKey,
        current: Option<&StateToken>,
    ) -> EvidenceLookup {
        self.stats.lookups += 1;
        match self.entries.get(key) {
            Some(stored) if stored.state.as_ref() == current => {
                self.stats.hits += 1;
                EvidenceLookup::Found(stored.observation.clone())
            }
            Some(_) => {
                self.stats.stale += 1;
                EvidenceLookup::Stale
            }
            None => {
                self.stats.misses += 1;
                EvidenceLookup::NotFound
            }
        }
    }

    pub(crate) fn invalidate(&mut self, capability: &CapabilityId) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|key, _| &key.capability_id != capability);
        before - self.entries.len()
    }

    pub(crate) fn stats(&self) -> EvidenceStats {
        self.stats
    }
}
