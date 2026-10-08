//! Identifiers an execution runtime reported for one execution.
//!
//! Chip does not create or derive runtime identity. When an executor reports
//! identifiers it obtained from the runtime that owns them, Chip carries the
//! normalized evidence alongside the execution's final record (`Observation`)
//! without interpreting it. The executor is responsible for obtaining
//! authoritative values. Absence is expected and meaningful: an execution with
//! no reported evidence carries none.
//!
//! Nothing here is ever derived. No identifier may be filled from a Chip
//! execution id, a work id, a path, a command, output, an error message or a
//! timestamp.
//!
//! Evidence is deliberately not part of [`crate::Observation::render`], the text
//! handed to the model: a model must not see, quote or forge these identifiers.

use std::collections::BTreeMap;

/// Evidence grouped by the runtime that reported it. The runtime's name is data
/// supplied by the executor; this crate knows no runtime by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutionEvidence {
    runtimes: BTreeMap<String, BTreeMap<String, String>>,
}

/// Names (runtime and identifier) are plain ASCII words, so evidence stays inert data.
fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl ExecutionEvidence {
    /// Evidence from one runtime. Blank values and invalid names are dropped and
    /// every other value is kept exactly as supplied. `None` when nothing is
    /// left: empty evidence is never carried.
    pub fn from_runtime<I, K, V>(runtime: &str, identifiers: I) -> Option<Self>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut evidence = Self::default();
        if valid_name(runtime) {
            for (key, value) in identifiers {
                let (key, value) = (key.into(), value.into());
                if valid_name(&key) && !value.trim().is_empty() {
                    evidence
                        .runtimes
                        .entry(runtime.to_string())
                        .or_default()
                        .insert(key, value);
                }
            }
        }
        (!evidence.runtimes.is_empty()).then_some(evidence)
    }

    /// Adds another runtime's evidence, replacing a runtime already present.
    pub fn merged(mut self, other: Self) -> Self {
        self.runtimes.extend(other.runtimes);
        self
    }

    /// The identifiers one runtime reported, if it reported any.
    pub fn runtime(&self, name: &str) -> Option<&BTreeMap<String, String>> {
        self.runtimes.get(name)
    }

    /// Every runtime that reported evidence, with its identifiers.
    pub fn runtimes(&self) -> impl Iterator<Item = (&str, &BTreeMap<String, String>)> {
        self.runtimes.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Re-normalizes evidence assembled by hand. `None` when it is empty.
    pub fn normalized(self) -> Option<Self> {
        self.runtimes
            .into_iter()
            .filter_map(|(runtime, ids)| Self::from_runtime(&runtime, ids))
            .reduce(Self::merged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_identifiers_exactly() {
        let e = ExecutionEvidence::from_runtime(
            "rt",
            [("executionId", "exec_789"), ("jobId", "job_456")],
        )
        .unwrap();
        let ids = e.runtime("rt").unwrap();
        assert_eq!(ids.get("executionId").map(String::as_str), Some("exec_789"));
        assert_eq!(ids.get("jobId").map(String::as_str), Some("job_456"));
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn partial_evidence_invents_nothing() {
        let e = ExecutionEvidence::from_runtime("rt", [("jobId", "job_456")]).unwrap();
        assert_eq!(
            e.runtime("rt").unwrap().keys().collect::<Vec<_>>(),
            ["jobId"]
        );
    }

    #[test]
    fn empty_blank_and_invalid_evidence_is_omitted() {
        let none: [(&str, &str); 0] = [];
        assert!(ExecutionEvidence::from_runtime("rt", none).is_none());
        assert!(
            ExecutionEvidence::from_runtime("rt", [("jobId", "  "), ("receiptId", "")]).is_none()
        );
        assert!(ExecutionEvidence::from_runtime("rt", [("not valid", "x"), ("1x", "y")]).is_none());
        assert!(ExecutionEvidence::from_runtime("bad name", [("jobId", "j")]).is_none());
        assert!(ExecutionEvidence::default().normalized().is_none());
    }

    #[test]
    fn normalizing_keeps_good_values_and_drops_the_rest() {
        let e =
            ExecutionEvidence::from_runtime("rt", [("jobId", "j"), ("receiptId", " ")]).unwrap();
        assert_eq!(e.clone().normalized(), Some(e));
    }
}
