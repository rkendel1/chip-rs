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

/// `NotFound` means no knowledge. A found failure is knowledge that it failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceLookup {
    Found(Observation),
    NotFound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EvidenceStats {
    pub lookups: usize,
    pub hits: usize,
    pub misses: usize,
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

#[derive(Debug, Default)]
pub(crate) struct EvidenceStore {
    entries: BTreeMap<EvidenceKey, Observation>,
    stats: EvidenceStats,
}

impl EvidenceStore {
    pub(crate) fn record(&mut self, key: EvidenceKey, observation: Observation) {
        self.entries.insert(key, observation);
    }

    pub(crate) fn lookup(&mut self, key: &EvidenceKey) -> EvidenceLookup {
        self.stats.lookups += 1;
        match self.entries.get(key) {
            Some(observation) => {
                self.stats.hits += 1;
                EvidenceLookup::Found(observation.clone())
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
